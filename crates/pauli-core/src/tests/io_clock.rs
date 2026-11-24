//! Device-clock routing with an explicit monotonic source; no timing sleeps.
//! Ownership: each fixture owns the simulated worker and management clock.
//! Invariant: only a timed I/O operation samples TIME, when it services the VM.
use super::model::Fixture;
use core::cell::Cell;
use pauli_config::GuestBoardLayout;
use pauli_hal::device::MmioAccess;

// Guest-visible Goldfish RTC register offsets, in bytes.
const TIME_LOW: usize = 0x00;
const ALARM_LOW: u64 = 0x08;
const ALARM_STATUS: usize = 0x18;

fn word(gpa: u64, write: Option<usize>) -> MmioAccess {
    MmioAccess {
        gpa: gpa as usize,
        width: 4,
        write,
    }
}

#[test]
fn untimed_interrupt_mmio_and_unmapped_addresses_do_not_sample_time() {
    let f = Fixture::new(1, 1);
    let m = &f.runtime.manager;
    let vm = m.vm(0).unwrap();
    let no_clock = || panic!("an untimed access sampled TIME");
    let plic = GuestBoardLayout::DEFAULT.plic.base;
    assert_eq!(
        vm.devices.access(word(plic, None), no_clock, m.link()),
        Some(0)
    );
    assert_eq!(vm.devices.access(word(0, None), no_clock, m.link()), None);
}

#[test]
fn unchanged_device_polls_do_not_sample_guest_irq_contexts() {
    let f = Fixture::new(2, 2);
    let m = &f.runtime.manager;
    let vm = m.vm(0).unwrap();
    let layout = GuestBoardLayout::DEFAULT;
    let samples = || vm.devices.irqs.lock().samples.get();
    let now = || 10;
    let poll = || vm.devices.service(now, m.link()).unwrap();
    let initial = samples();
    for _ in 0..8 {
        poll();
        assert!(
            vm.devices
                .access(word(layout.rtc.base, None), now, m.link())
                .is_some()
        );
    }
    assert_eq!(samples(), initial);

    // An actual RX transition must still reach the chip; an unchanged high
    // line is just as quiet on subsequent polls as an unchanged low one.
    let ier = MmioAccess {
        gpa: layout.uart.base as usize + 1,
        width: 1,
        write: Some(1),
    };
    assert!(vm.devices.access(ier, now, m.link()).is_some());
    assert!(vm.devices.receive_console(b'x', now));
    let raised = samples();
    assert!(raised > initial);
    poll();
    assert_eq!(samples(), raised);
}

#[test]
fn maintenance_uses_the_live_clock_even_if_requested_before_a_guest_rtc_write() {
    let f = Fixture::new(2, 1);
    let m = &f.runtime.manager;
    let vm = m.vm(0).unwrap();
    let rtc = GuestBoardLayout::DEFAULT.rtc.base;
    let ticks = Cell::new(10);
    let now = || ticks.get();
    let poll = || vm.devices.service(now, m.link()).unwrap();
    // A manager pass can start before a worker, but service the VM after it.
    // Reusing the manager's original 10 us would wrap the RTC's elapsed delta.
    ticks.set(100);
    assert_eq!(
        vm.devices
            .access(word(rtc + ALARM_LOW, Some(200_000)), now, m.link()),
        Some(0)
    );
    ticks.set(150);
    poll();
    let mut io = vm.devices.io.lock();
    assert_eq!(io.rtc.access(TIME_LOW, None), Some(150_000)); // 1 MHz fixture, RTC in ns.
    assert_eq!(io.rtc.access(ALARM_STATUS, None), Some(1)); // The 200 us alarm is still armed.
    drop(io);
    ticks.set(200);
    poll();
    assert_eq!(vm.devices.io.lock().rtc.access(ALARM_STATUS, None), Some(0));
}
