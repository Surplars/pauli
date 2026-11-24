//! HS console ingress and the guest MMIO consumer share the real UART and IRQ bridge.
//! The model observes level transitions and owner kicks; it does not emulate a PLIC.
use super::model::{
    Fixture, IDLE_DEADLINE, IDLE_KICK, KICKED_OWNERS, KICKS, NOW, OWNER, REJECT_KICK_ONCE,
};
use crate::{arch::ExitKind, monitor::backend::Target, runtime::Dispatcher};
use pauli_control::{Backend, VmId};
use pauli_hal::device::MmioAccess;

#[cfg(feature = "selftest")]
mod ack;
mod owner;

const DATA: usize = 0;
const IER: usize = 1;
const IIR_FCR: usize = 2;
const LSR: usize = 5;

fn pair() -> Fixture {
    let f = Fixture::with_config(2, [1; 2], 0, |_, vm| {
        vm.firmware = Some(pauli_config::FirmwareLayout {
            rx_pages: 1,
            ro_pages: 0,
        });
        vm.memory_size = 32 * 1024;
        vm.cpu_affinity = pauli_config::CpuAffinity::Workers;
    });
    f.runtime.manager.start(f.runtime.board, 0).unwrap();
    KICKS.with(|k| k.set(0));
    KICKED_OWNERS.with(|k| k.set(0));
    f
}

fn reg(f: &Fixture, offset: usize, write: Option<usize>) -> usize {
    let vm = f.runtime.manager.vm(0).unwrap();
    let access = MmioAccess {
        gpa: pauli_config::GuestBoardLayout::DEFAULT.uart.base as usize + offset,
        width: 1,
        write,
    };
    vm.devices
        .access(access, || NOW.with(|n| n.get()), f.runtime.manager.link())
        .unwrap()
}

#[test]
fn ingress_samples_the_publication_clock_only_for_a_byte_caused_rx_edge() {
    use core::cell::Cell;
    let f = pair();
    let vm = f.runtime.manager.vm(0).unwrap();
    let reads = Cell::new(0);
    let clock = || {
        reads.set(reads.get() + 1);
        reads.get()
    };
    reg(&f, IIR_FCR, Some(1));
    reg(&f, IER, Some(1));
    assert!(vm.devices.receive_console(b'a', clock)); // Low -> high.
    assert!(vm.devices.receive_console(b'b', clock)); // Already high.
    let extra = u64::from(cfg!(feature = "selftest"));
    assert_eq!(reads.get(), 2 + extra);
    #[cfg(feature = "selftest")]
    {
        let rise = vm.devices.irqs.lock().rx.last.unwrap();
        assert_eq!((rise.seq, rise.byte, rise.ticks), (1, b'a', 2));
        let peer = f.runtime.manager.vm(1).unwrap();
        assert!(peer.devices.irqs.lock().rx.last.is_none());
    }
    for byte in b"ab" {
        assert_eq!(reg(&f, DATA, None), usize::from(*byte));
    }
    // THRE owns the line: an accepted byte is not necessarily a new assertion.
    reg(&f, IER, Some(3));
    assert!(vm.devices.receive_console(b'x', clock));
    assert_eq!(reads.get(), 3 + extra);
    reg(&f, DATA, None);
    reg(&f, IER, Some(1));
    assert!(vm.devices.receive_console(b'y', clock));
    assert_eq!(reads.get(), 4 + 2 * extra);
    #[cfg(feature = "selftest")]
    {
        let rise = vm.devices.irqs.lock().rx.last.unwrap();
        assert_eq!((rise.seq, rise.byte, rise.ticks), (2, b'y', 6));
    }
}

#[test]
fn input_wakes_only_the_waiting_owner_without_a_service_poll() {
    let f = pair();
    let rt = &f.runtime;
    reg(&f, IIR_FCR, Some(1)); // FIFO on, interrupt after one byte.
    reg(&f, IER, Some(1));
    let vm = rt.manager.vm(0).unwrap();
    let peer = rt.manager.vm(1).unwrap();
    let cpu = vm.cpu(0).unwrap();
    cpu.data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Wait);
    let mut local = Dispatcher::new();
    OWNER.with(|o| o.set(1));
    assert!(rt.dispatch(&mut local).unwrap());
    assert!(!rt.dispatch(&mut local).unwrap());
    OWNER.with(|o| o.set(0));
    for byte in b"ab" {
        Target(rt).input(VmId(1), *byte).unwrap();
    }
    assert!(vm.devices.irqs.lock().lines[0]);
    assert!(!peer.devices.irqs.lock().lines[0]);
    assert_eq!(KICKS.with(|k| k.get()), 1); // Second byte leaves the same level high.
    assert_eq!(KICKED_OWNERS.with(|k| k.get()), 1 << 1);
    OWNER.with(|o| o.set(1));
    // Input raced the last empty scan, before masked WFI. A native kick alone
    // must be sufficient: the sleeping worker has no maintenance or guest timer.
    rt.idle(&mut local).unwrap();
    assert!(IDLE_KICK.with(|k| k.get()));
    assert_eq!(IDLE_DEADLINE.with(|d| d.get()), None);
    #[cfg(feature = "selftest")]
    assert_eq!(rt.worker_wait(1), Some((false, 1)));
    assert!(rt.dispatch(&mut local).unwrap());
    assert!(cpu.data.lock().context.as_ref().unwrap().external);
    assert_eq!(reg(&f, DATA, None), b'a' as usize);
    assert_eq!(reg(&f, DATA, None), b'b' as usize);
    assert!(!vm.devices.irqs.lock().lines[0]);
}

#[test]
fn a_rejected_rx_or_stop_kick_is_retried_before_join() {
    let f = pair();
    let rt = &f.runtime;
    let vm = rt.manager.vm(0).unwrap();
    reg(&f, IER, Some(1));
    REJECT_KICK_ONCE.with(|k| k.set(Some(1)));
    Target(rt).input(VmId(1), b'x').unwrap();
    assert_eq!(KICKS.with(|k| k.get()), 1);
    rt.manager.service(rt.board);
    assert_eq!(KICKS.with(|k| k.get()), 2);
    let mut local = Dispatcher::new();
    OWNER.with(|o| o.set(1));
    assert!(rt.dispatch(&mut local).unwrap());
    assert!(
        vm.cpu(0)
            .unwrap()
            .data
            .lock()
            .context
            .as_ref()
            .unwrap()
            .external
    );

    // A failed physical stop notification cannot validate retirement.
    OWNER.with(|o| o.set(0));
    REJECT_KICK_ONCE.with(|k| k.set(Some(1)));
    vm.host_stop();
    let rejected = KICKS.with(|k| k.get());
    rt.manager.service(rt.board);
    assert_eq!(KICKS.with(|k| k.get()), rejected + 1);
    assert!(!vm.lifecycle.joined());
    OWNER.with(|o| o.set(1));
    assert!(!rt.dispatch(&mut local).unwrap());
    OWNER.with(|o| o.set(0));
    rt.manager.service(rt.board);
    assert!(vm.lifecycle.joined());
}

#[test]
fn fifo_timeout_starts_at_ingress_and_an_expired_level_precedes_rearm() {
    let f = pair();
    let rt = &f.runtime;
    let vm = rt.manager.vm(0).unwrap();
    reg(&f, IIR_FCR, Some(0x41)); // Four-byte threshold, 1 ms timeout at 1 MHz.
    reg(&f, IER, Some(1));
    NOW.with(|n| n.set(20_000));
    Target(rt).input(VmId(1), b'a').unwrap();
    NOW.with(|n| n.set(20_999));
    rt.manager.service(rt.board);
    assert_eq!(vm.devices.irqs.lock().uart_assertions, 0);
    assert_eq!(KICKS.with(|k| k.get()), 0);

    // No intervening poll: latch the old timeout at 21,000 before this new
    // byte moves the below-threshold FIFO deadline to 22,000.
    NOW.with(|n| n.set(21_000));
    Target(rt).input(VmId(1), b'b').unwrap();
    assert_eq!(vm.devices.irqs.lock().uart_assertions, 1);
    #[cfg(feature = "selftest")]
    assert!(vm.devices.irqs.lock().rx.last.is_none());
    assert!(!vm.devices.irqs.lock().lines[0]);
    NOW.with(|n| n.set(21_999));
    assert_eq!(reg(&f, IIR_FCR, None) & 0x0f, 1);
    NOW.with(|n| n.set(22_000));
    rt.manager.service(rt.board);
    assert!(vm.devices.irqs.lock().lines[0]);
    assert_eq!(reg(&f, IIR_FCR, None) & 0x0f, 12); // Receive timeout.
}

#[test]
fn rejected_byte_can_raise_line_status_without_overwriting_the_fifo() {
    let f = pair();
    let rt = &f.runtime;
    let vm = rt.manager.vm(0).unwrap();
    reg(&f, IER, Some(4)); // Only line-status IRQ, single-byte holding register.
    Target(rt).input(VmId(1), b'a').unwrap();
    assert_eq!(KICKS.with(|k| k.get()), 0);
    assert!(Target(rt).input(VmId(1), b'b').is_err());
    #[cfg(feature = "selftest")]
    assert!(vm.devices.irqs.lock().rx.last.is_none());
    assert!(vm.devices.irqs.lock().lines[0]);
    assert_eq!(KICKS.with(|k| k.get()), 1);
    assert_eq!(reg(&f, IIR_FCR, None) & 0x0f, 6); // Line status has priority.
    assert_ne!(reg(&f, LSR, None) & 2, 0); // Reading LSR acknowledges overrun.
    assert!(!vm.devices.irqs.lock().lines[0]);
    assert_eq!(reg(&f, DATA, None), b'a' as usize);
    assert_eq!(reg(&f, LSR, None) & 1, 0);
}

#[test]
fn cold_restart_drops_unread_rx_and_rejects_input_from_an_old_attachment() {
    use pauli_control::{Caller, Error, Operation, Request, Service};

    let f = pair();
    let rt = &f.runtime;
    let vm = rt.manager.vm(0).unwrap();
    let mut service = Service::new();
    let mut target = Target(rt);
    let input = Request {
        vm: VmId(1),
        epoch: vm.epoch(),
        operation: Operation::Input(b'x'),
    };
    reg(&f, IER, Some(1));
    service.execute(Caller::Host, input, &mut target).unwrap();
    assert!(vm.devices.irqs.lock().lines[0]);
    #[cfg(feature = "selftest")]
    {
        let mut bank = vm.devices.irqs.lock();
        assert!(bank.owner_pending::<crate::tests::model::Smp>(0, 1, || 20));
        let rise = bank.rx.last.unwrap();
        assert_eq!((rise.seq, rise.owner.unwrap().ticks), (1, 20));
    }

    vm.host_stop();
    OWNER.with(|o| o.set(1)); // Retire on the pinned worker, join on the manager.
    rt.dispatch(&mut Dispatcher::new()).unwrap();
    OWNER.with(|o| o.set(0));
    rt.manager.service(rt.board);
    rt.manager.start(rt.board, 0).unwrap();
    assert_ne!(vm.epoch(), input.epoch);
    assert!(!vm.devices.irqs.lock().lines[0]);
    #[cfg(feature = "selftest")]
    assert!(vm.devices.irqs.lock().rx.last.is_none());
    assert_eq!(reg(&f, IER, None), 0);
    reg(&f, IER, Some(1));
    assert_eq!(
        service.execute(Caller::Host, input, &mut target),
        Err(Error::StaleEpoch)
    );
    assert_eq!(reg(&f, LSR, None) & 1, 0);
    service
        .execute(
            Caller::Host,
            Request {
                epoch: vm.epoch(),
                operation: Operation::Input(b'y'),
                ..input
            },
            &mut target,
        )
        .unwrap();
    assert!(vm.devices.irqs.lock().lines[0]);
    #[cfg(feature = "selftest")]
    {
        let rise = vm.devices.irqs.lock().rx.last.unwrap();
        assert_eq!((rise.seq, rise.byte), (1, b'y'));
        assert!(rise.owner.is_none()); // No observation from the previous incarnation.
    }
    assert_eq!(reg(&f, DATA, None), b'y' as usize);
}
