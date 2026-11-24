//! Manager idle decisions for real guest RTC programming on the 1 MHz fixture.
//! One owner drives MMIO and maintenance; an alarm belongs only to its live VM.
use super::{Dispatcher, Fixture, NOW, Timer, deadline, guests, idle, park};
use pauli_config::GuestBoardLayout;
use pauli_hal::device::MmioAccess;

const ALARM_LOW: u64 = 0x08;
const IRQ_ENABLE: u64 = 0x10;
const CLEAR_ALARM: u64 = 0x14;

fn parked_pair() -> (Fixture, Dispatcher<Timer>) {
    let f = guests(1, false);
    let mut local = Dispatcher::new();
    park(&f, 0, &mut local);
    park(&f, 1, &mut local);
    NOW.with(|n| n.set(1000));
    (f, local)
}

fn write(f: &Fixture, slot: usize, address: u64, value: usize) {
    let m = &f.runtime.manager;
    let access = MmioAccess {
        gpa: address as usize,
        width: 4,
        write: Some(value),
    };
    assert_eq!(
        m.vm(slot)
            .unwrap()
            .devices
            .access(access, || NOW.with(|n| n.get()), m.link()),
        Some(0)
    );
}

#[test]
fn rtc_and_peer_timer_compete_with_the_correct_maintenance_bound() {
    let (f, mut local) = parked_pair();
    let layout = GuestBoardLayout::DEFAULT;
    let rtc = layout.rtc.base;
    write(&f, 1, rtc + IRQ_ENABLE, 1);
    write(&f, 1, rtc + ALARM_LOW, 4_000_000);
    assert_eq!(idle(&f, &mut local), Some(2000)); // Far alarm: 1 ms monitor cap.
    write(&f, 1, rtc + ALARM_LOW, 1_500_001);
    assert_eq!(idle(&f, &mut local), Some(1501)); // Never round the nanoseconds down.
    deadline(&f, 0, Some(1300));
    assert_eq!(idle(&f, &mut local), Some(1300));

    // Work in VM0 must not hide an earlier RTC in VM1.
    write(&f, 0, layout.uart.base, b'x'.into());
    assert_eq!(idle(&f, &mut local), Some(1100)); // TX queue: 100 us cap.
    write(&f, 1, rtc + ALARM_LOW, 1_040_000);
    assert_eq!(idle(&f, &mut local), Some(1040));
    NOW.with(|n| n.set(1040));
    assert_eq!(idle(&f, &mut local), None); // Service the due source without WFI.
    let m = &f.runtime.manager;
    m.service(f.runtime.board);
    assert!(m.vm(1).unwrap().devices.irqs.lock().lines[0]);
    assert!(!m.vm(0).unwrap().devices.irqs.lock().lines[0]);
}

#[test]
fn masked_cancelled_and_restarted_alarms_do_not_shorten_a_peer_wait() {
    let (f, mut local) = parked_pair();
    let rtc = GuestBoardLayout::DEFAULT.rtc.base;
    let m = &f.runtime.manager;
    deadline(&f, 1, Some(1800));
    write(&f, 0, rtc + ALARM_LOW, 1_500_000);
    assert_eq!(idle(&f, &mut local), Some(1800)); // Armed while masked.
    write(&f, 0, rtc + IRQ_ENABLE, 1);
    assert_eq!(idle(&f, &mut local), Some(1500));
    write(&f, 0, rtc + CLEAR_ALARM, 1);
    assert_eq!(idle(&f, &mut local), Some(1800));
    write(&f, 0, rtc + ALARM_LOW, 1_400_000);
    assert_eq!(idle(&f, &mut local), Some(1400));

    let vm = m.vm(0).unwrap();
    let epoch = vm.epoch();
    vm.host_stop();
    f.runtime.dispatch(&mut local).unwrap();
    assert!(vm.try_join(1000, m.link()).unwrap());
    m.start(f.runtime.board, 0).unwrap();
    assert_ne!(epoch, vm.epoch());
    park(&f, 0, &mut local);
    assert_eq!(idle(&f, &mut local), Some(1800));
    NOW.with(|n| n.set(1400));
    m.service(f.runtime.board);
    assert!(
        !vm.devices.irqs.lock().lines[0],
        "old alarm fired in replacement VM"
    );
}

#[cfg(feature = "selftest")]
#[test]
fn an_rtc_only_compare_is_not_a_late_vcpu_timer_sample() {
    use super::super::model::IDLE_RETURN;
    use crate::arch::IdleWake;
    let (f, mut local) = parked_pair();
    let rtc = GuestBoardLayout::DEFAULT.rtc.base;
    deadline(&f, 1, Some(1900));
    write(&f, 0, rtc + ALARM_LOW, 1_200_000);
    write(&f, 0, rtc + IRQ_ENABLE, 1);
    // The RTC won the native compare. Even a return after the *other* deadline
    // is not lateness measured against a selected vCPU timer.
    IDLE_RETURN.with(|r| r.set(Some((2500, IdleWake::Timer))));
    assert_eq!(idle(&f, &mut local), Some(1200));
    let (causes, bins) = f.runtime.idle_wakes().unwrap();
    assert_eq!(causes[IdleWake::Timer as usize], 1);
    assert_eq!(bins, [0; 4]);
}
