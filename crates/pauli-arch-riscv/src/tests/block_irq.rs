//! Production algorithm acceptance, using private host fixtures.
//! Ownership/Lock Ordering: each test owns its state; no actual CSR access.
//! Invariants: retain the original protocol assertions and rejection cases.
use crate::{interrupt, plic};
use interrupt::VirtualInterruptController;
use plic::VirtualPlic;
fn controller() -> VirtualPlic {
    let mut p = VirtualPlic::with_contexts(2);
    p.write_word(4, 3).unwrap();
    p.write_word(40, 2).unwrap();
    p.write_word(44, 1).unwrap();
    p.write_word(0x2000, (1 << 1) | (1 << 10) | (1 << 11))
        .unwrap();
    p.write_word(0x2080, 1 << 1).unwrap();
    p
}
#[test]
fn acceptance() {
    uart_local_refresh();
    plic::check_model();
    let mut a = controller();
    let mut peer = controller();
    a.set_level(10, true);
    a.set_level(11, true);
    peer.set_level(1, true);
    a.set_level(1, true); // Only refresh block's line, not UART/RTC/peer state.
    assert!(a.source_pending_for(0, 1) && a.source_pending_for(1, 1));
    a.write_word(0x200000, 3).unwrap();
    assert!(!a.source_pending_for(0, 1));
    a.write_word(0x200000, 0).unwrap();
    assert_eq!(a.read_word(0x200004), Some(1));
    assert!(!a.source_pending_for(0, 1) && !a.source_pending_for(1, 1));
    assert_eq!(a.read_word(0x201004), Some(0)); // One claim owner, not two.
    a.set_level(1, false); // VirtIO ACK lowers the line; completion won't rearm.
    a.write_word(0x200004, 1).unwrap();
    assert_eq!(a.read_word(0x1000), Some((1 << 10) | (1 << 11)));
    assert_eq!(a.read_word(0x200004), Some(10));
    assert_eq!(peer.read_word(0x200004), Some(1));
    a.set_level(1, true);
    a.set_level(1, false);
    // Reset/deassert before a claim cannot silently discard a latched gateway.
    assert_eq!(a.read_word(0x200004), Some(1));
    a.write_word(0x200004, 1).unwrap();
    assert_eq!(a.read_word(0x201004), Some(0));
    assert_eq!(a.read_word(0x200004), Some(11));
    a.set_level(1, true);
    assert_eq!(a.read_word(0x201004), Some(1));
    a.write_word(0x201004, 1).unwrap(); // Held level re-arms at complete.
    assert_eq!(a.read_word(0x201004), Some(1));
    a.retire_source(1); // Joined stop must clear pending and in-service state.
    assert_eq!(a.read_word(0x201004), Some(0));
    std::println!(
        "Block IRQ PASS: local line refresh, ACK/claim/complete, reset latch, two contexts, peer/source isolation and joined retirement"
    );
}

fn uart_local_refresh() {
    use pauli_devices::uart16550::Uart16550;
    let mut uart = Uart16550::new(10);
    let mut p = controller();
    let mut peer = controller();
    p.set_level(1, true);
    p.set_level(11, true);
    peer.set_level(10, true);
    uart.write(2, 0xc1); // RX trigger 14; one byte will use character timeout.
    uart.write(1, 1);
    assert!(uart.receive(b'x'));
    assert!(!uart.irq_pending());
    uart.advance(10);
    p.set_level(10, uart.irq_pending()); // Latch before a destructive read.
    assert_eq!(uart.read(0), Some(b'x'));
    p.set_level(10, uart.irq_pending());
    // Polling away the byte must not erase the already-latched IRQ gateway,
    // and UART-local updates cannot ACK block, RTC or the other VM's source.
    assert_eq!(p.read_word(0x200004), Some(1));
    p.set_level(1, false);
    p.write_word(0x200004, 1).unwrap();
    assert_eq!(p.read_word(0x200004), Some(10));
    p.write_word(0x200004, 10).unwrap();
    assert_eq!(p.read_word(0x200004), Some(11));
    assert_eq!(peer.read_word(0x200004), Some(10));
    uart.write(1, 2); // THRE is immediate, IIR alone acknowledges its latch.
    p.set_level(10, uart.irq_pending());
    assert_eq!(uart.read(2), Some(0xc2));
    p.set_level(10, uart.irq_pending());
    assert!(!uart.irq_pending());
    assert_eq!(p.read_word(0x200004), Some(10));
}
