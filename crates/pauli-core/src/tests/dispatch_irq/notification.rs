//! Production device mutation/notification with the test controller's vCPU0 inbox.
//! IRQ file ownership can consume pending state without a call to a device producer.
use super::Fixture;
use crate::{arch::GuestIo, trace::NoopTracer, vm::devices::DeviceIo};
use core::sync::atomic::Ordering;
use pauli_config::GuestBoardLayout;
use pauli_hal::device::MmioAccess;

fn reg(gpa: u64, width: usize, write: Option<usize>) -> MmioAccess {
    MmioAccess {
        gpa: gpa as usize,
        width,
        write,
    }
}

#[test]
fn remote_irq_after_native_consumption_needs_no_intervening_poll() {
    let f = Fixture::new(2, 1);
    let m = &f.runtime.manager;
    let vm = m.vm(0).unwrap();
    let dev = &vm.devices;
    let board = GuestBoardLayout::DEFAULT;
    for op in [
        reg(board.uart.base + 1, 1, Some(1)), // Enable RX before the first interrupt.
        reg(board.rtc.base + 0x10, 4, Some(1)),
        reg(board.rtc.base + 0x08, 4, Some(0)), // Already-due RTC alarm.
    ] {
        assert_eq!(dev.access(op, || 0, m), Some(0));
    }
    assert_eq!(dev.wake.swap(0, Ordering::AcqRel), 1);
    // Stand-in for a resident owner draining its file, outside the producer path.
    dev.irqs.lock().lines[0] = false;
    assert!(dev.receive_console(b'B', || 1));
    assert_eq!(dev.wake.swap(0, Ordering::AcqRel), 1);
    assert_eq!(m.vm(1).unwrap().devices.wake.load(Ordering::Acquire), 0);
}

#[test]
fn destructive_guest_uart_read_notifies_a_colocated_sibling_but_not_itself() {
    let f = Fixture::new(1, 2);
    let m = &f.runtime.manager;
    let vm = m.vm(0).unwrap();
    assert_eq!(vm.cpu(0).unwrap().owner, vm.cpu(1).unwrap().owner);
    let dev = &vm.devices;
    let uart = GuestBoardLayout::DEFAULT.uart.base;
    assert_eq!(dev.access(reg(uart + 1, 1, Some(1)), || 0, m), Some(0));
    for (origin, falling_wakes) in [(0, 0), (1, 1)] {
        assert!(dev.receive_console(b'R', || 0));
        assert_eq!(dev.wake.swap(0, Ordering::AcqRel), 1);
        let mut io = DeviceIo {
            devices: dev,
            origin,
            manager: m,
            now: || 0,
            trace: &mut NoopTracer,
            console_tx: false,
            console_full: false,
            service: None,
        };
        assert_eq!(io.mmio(reg(uart, 1, None)), Some(b'R' as usize));
        assert_eq!(dev.wake.swap(0, Ordering::AcqRel), falling_wakes);
    }
}
