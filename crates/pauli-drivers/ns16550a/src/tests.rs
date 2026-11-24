//! UART byte/word MMIO layout checks on private RAM, not real UART registers.
//! Ownership/Lock Ordering: local fixture, no concurrency or live IRQs.
//! Invariants: reject unsupported stride/width before touching any register.

use super::*;
struct FakeIrq;
// SAFETY: local RAM fixture has no actual interrupt source.
unsafe impl LocalIrq for FakeIrq {
    type Saved = ();
    fn save() {}
    unsafe fn restore(_: ()) {}
}

#[test]
fn queued_hint_preserves_input_after_the_native_irq_has_cleared() {
    let mut words = [0u32; 8];
    let resource = DeviceResource {
        base: words.as_mut_ptr() as usize,
        size: size_of_val(&words),
        irq: None,
    };
    // SAFETY: private word-aligned registers remain live for this test.
    let uart = unsafe { Ns16550a::<FakeIrq>::new(resource, 2, 4, 0).unwrap() };
    uart.write(LSR, LSR_DR).unwrap();
    assert!(!uart.has_rx(), "readiness is not a hardware FIFO poll");
    // This fixed LSR supplies IRQ_BYTES identical bytes per bounded ISR call.
    for byte in b"ab" {
        uart.write(RBR, *byte).unwrap();
        assert!(uart.handle_irq());
    }
    uart.write(LSR, 0).unwrap();
    assert!(!uart.handle_irq());
    for byte in b"ab" {
        for _ in 0..IRQ_BYTES {
            assert!(uart.has_rx());
            assert_eq!(uart.read_byte(), Some(*byte));
        }
    }
    assert!(!uart.has_rx());
    assert_eq!(uart.read_byte(), None);
}

#[test]
fn word_stride_and_bounded_tx() {
    let mut words = [0u32; 8];
    let resource = DeviceResource {
        base: words.as_mut_ptr() as usize,
        size: 32,
        irq: None,
    };
    // SAFETY: private word-aligned RAM remains alive throughout the test.
    let uart = unsafe { Ns16550a::<FakeIrq>::new(resource, 2, 4, 3).unwrap() };
    assert_eq!(words[LCR], LCR_8N1 as u32);
    assert_eq!(words[IER], (IER_RX | IER_LINE) as u32);
    assert_eq!(uart.write_byte(b'x'), Err(DeviceError::Busy));
    words[LSR] = LSR_THRE as u32;
    assert_eq!(uart.write_byte(b'x'), Ok(()));
    assert_eq!(words[THR], b'x' as u32);
    // SAFETY: invalid layout is rejected before access to this same live fixture.
    assert!(unsafe { Ns16550a::<FakeIrq>::new(resource, 0, 4, 0) }.is_err());
}
