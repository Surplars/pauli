//! Exercise the physical UART as Linux sees it, with a second boot slot sharing the hart.
use super::{model::Fixture, uart_capture::capture};
use crate::{monitor::Monitor, runtime::Dispatcher};
use core::sync::atomic::Ordering;

fn setup() -> (Fixture, Monitor) {
    let f = Fixture::with_config(1, [1, 1], 0, |slot, vm| vm.autostart = slot == 0);
    let rt = &f.runtime;
    rt.board.direct_linux.store(true, Ordering::Relaxed);
    // Boot::start_workers normally starts autostart slots before Monitor::welcome.
    rt.manager.start(rt.board, 0).unwrap();
    let mut monitor = Monitor::new();
    assert!(
        capture(|| {
            monitor.welcome(rt);
            monitor.poll(rt);
        })
        .is_empty(),
        "no host prompt, attach or epoch frames"
    );
    (f, monitor)
}

#[test]
fn linux_owns_the_wire_and_peer_tx_still_drains() {
    let (f, mut monitor) = setup();
    let rt = &f.runtime;
    rt.manager.start(rt.board, 1).unwrap();
    let linux = rt.manager.vm(0).unwrap();
    let peer = rt.manager.vm(1).unwrap();
    let wire = b"\x00\x1d\x1e\xff\r\n";
    for (slot, bytes) in [(0, wire.as_slice()), (1, b"private peer log")] {
        let vm = rt.manager.vm(slot).unwrap();
        let mut io = vm.devices.io.lock();
        for &byte in bytes {
            io.console.access(0, Some(byte.into())).unwrap();
        }
    }
    // Even with no FIFO enabled the entire native burst must survive across polls.
    // The host's escape and line editor are never in this route.
    rt.board.input.lock().extend(wire);
    for (i, &byte) in wire.iter().enumerate() {
        let out = capture(|| monitor.poll(rt));
        assert_eq!(out.as_slice(), if i == 0 { wire.as_slice() } else { &[] });
        assert_eq!(
            linux.devices.io.lock().console.access(0, None),
            Some(byte.into())
        );
    }
    assert!(!peer.devices.io.lock().console.has_output());
    assert_eq!(
        peer.devices.io.lock().console.read_log(0),
        Ok(pauli_control::ConsoleChunk::Bytes {
            data: *b"privat",
            len: 6
        })
    );
    assert_eq!(
        linux.devices.io.lock().console.read_log(0),
        Ok(pauli_control::ConsoleChunk::Bytes {
            data: *wire,
            len: 6
        })
    );
    assert!(rt.board.input.lock().is_empty());
    assert_eq!(monitor.terminal.line(&mut [0; 160]), Some(0));
}

#[test]
fn primary_fault_is_visible_without_opening_host_shell_then_restart_rebinds() {
    let (f, mut monitor) = setup();
    let rt = &f.runtime;
    let primary = rt.manager.vm(0).unwrap();
    let old = primary.epoch();
    primary.request_stop(true, false);
    rt.dispatch(&mut Dispatcher::new()).unwrap();
    rt.board.input.lock().extend(b"shutdown\r");
    let out = capture(|| monitor.poll(rt));
    assert_eq!(
        out,
        std::format!("\r\n[HV] VM 1 epoch={old} faulted\r\n").as_bytes()
    );
    assert!(rt.board.input.lock().is_empty());
    assert!(capture(|| monitor.poll(rt)).is_empty());
    rt.manager.start(rt.board, 0).unwrap();
    assert_ne!(primary.epoch(), old);
    rt.board.input.lock().push_back(b'z');
    assert!(capture(|| monitor.poll(rt)).is_empty());
    assert_eq!(
        primary.devices.io.lock().console.access(0, None),
        Some(b'z'.into())
    );
}
