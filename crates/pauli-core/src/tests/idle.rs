//! Native WFI decisions with two parked guests and a 1 MHz fixture clock.
//! Ownership: one simulated owner drives the real dispatcher, idle and restart paths.
//! Invariant: a saved timer is usable only for the current, waiting incarnation.
use super::model::{Fixture, IDLE_DEADLINE, IDLE_ENTRIES, NOW, Timer};
use crate::{arch::ExitKind, runtime::Dispatcher, vm::slot::CpuState};

mod console_input;
mod exclusive;
mod joined;
mod overcommit;
mod rtc;
mod shared_deadline;
mod smp_mix;

fn guests(harts: usize, linux_peer: bool) -> Fixture {
    guests_on(harts, linux_peer, pauli_config::CpuAffinity::Automatic, 0)
}

fn guests_on(
    harts: usize,
    linux_peer: bool,
    affinity: pauli_config::CpuAffinity,
    spare_mib: usize,
) -> Fixture {
    let f = Fixture::with_config(harts, [1; 2], spare_mib, |slot, vm| {
        vm.cpu_affinity = affinity;
        if slot == 0 || !linux_peer {
            vm.memory_size = 32 * 1024;
            vm.firmware = Some(pauli_config::FirmwareLayout {
                rx_pages: 1,
                ro_pages: 0,
            });
        }
    });
    for slot in 0..2 {
        f.runtime.manager.start(f.runtime.board, slot).unwrap();
    }
    f
}

fn park(f: &Fixture, slot: usize, local: &mut Dispatcher<Timer>) {
    let vm = f.runtime.manager.vm(slot).unwrap();
    let cpu = vm.cpu(0).unwrap();
    cpu.data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Wait);
    super::model::OWNER.with(|owner| owner.set(cpu.owner));
    assert!(f.runtime.dispatch(local).unwrap());
    assert_eq!(cpu.data.lock().state, CpuState::Waiting);
}

fn deadline(f: &Fixture, slot: usize, deadline: Option<u64>) {
    let vm = f.runtime.manager.vm(slot).unwrap();
    vm.cpu(0)
        .unwrap()
        .data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .deadline = deadline;
}

/// None means no WFI was executed, not just that the timer was disarmed.
fn idle(f: &Fixture, local: &mut Dispatcher<Timer>) -> Option<u64> {
    IDLE_ENTRIES.with(|n| n.set(0));
    f.runtime.idle(local).unwrap();
    match IDLE_ENTRIES.with(|n| n.get()) {
        0 => None,
        1 => Some(IDLE_DEADLINE.with(|d| d.get().unwrap())),
        _ => panic!("idle entered WFI more than once"),
    }
}

#[test]
fn waiting_firmware_uses_earliest_live_timer_without_sleeping_past_expiry() {
    let f = guests(1, false);
    let mut local = Dispatcher::new();
    park(&f, 0, &mut local);
    park(&f, 1, &mut local);
    NOW.with(|n| n.set(1000));
    for (a, b, expected) in [
        (None, None, Some(2000)), // An IRQ-driven idle console keeps the 1 ms bound.
        (None, Some(5000), Some(2000)), // Never postpone maintenance beyond 1 ms.
        (Some(1740), Some(1250), Some(1250)),
        (Some(1010), Some(1740), Some(1010)), // Also shorten the old 100 us poll.
        (Some(1000), None, None),
        (None, Some(999), None),
    ] {
        deadline(&f, 0, a);
        deadline(&f, 1, b);
        assert_eq!(idle(&f, &mut local), expected, "timers {a:?}, {b:?}");
    }
    assert!(f.runtime.dispatch(&mut local).unwrap()); // The due peer gets the owner back.
}

#[cfg(feature = "selftest")]
#[test]
fn only_elapsed_guest_compares_with_timer_only_returns_enter_lateness_bins() {
    use super::model::IDLE_RETURN;
    use crate::arch::IdleWake::{External, Multiple, Timer};
    let f = guests(1, false);
    let mut local = Dispatcher::new();
    park(&f, 0, &mut local);
    park(&f, 1, &mut local);

    let (mut causes, mut bins) = ([0; 5], [0; 4]);
    // Each row advances the 1 MHz clock: requested wait and actual WFI duration in us.
    for (wait, elapsed, source, bucket) in [
        (600, 599, Timer, None),      // Pending bit alone does not establish expiry.
        (1000, 1000, Timer, Some(0)), // Guest deadline ties the maintenance cap.
        (600, 1599, Timer, Some(0)),
        (600, 1600, Timer, Some(1)),
        (600, 5599, Timer, Some(1)),
        (600, 5600, Timer, Some(2)),
        (600, 10599, Timer, Some(2)),
        (600, 10600, Timer, Some(3)),
        (1001, 15000, Timer, None), // Cap fired; guest deadline elapsed in the meantime.
        (600, 15000, External, None),
        (600, 15000, Multiple, None),
    ] {
        let start = NOW.with(|n| n.get());
        deadline(&f, 1, Some(start + wait));
        IDLE_RETURN.with(|r| r.set(Some((start + elapsed, source))));
        assert_eq!(idle(&f, &mut local), Some(start + wait.min(1000)));
        causes[source as usize] += 1;
        if let Some(i) = bucket {
            bins[i] += 1;
        }
        assert_eq!(f.runtime.idle_wakes(), Some((causes, bins)));
    }
    let start = NOW.with(|n| n.get());
    deadline(&f, 1, Some(start)); // Already due: neither a WFI return nor a latency sample.
    assert_eq!(idle(&f, &mut local), None);
    assert_eq!(f.runtime.idle_wakes(), Some((causes, bins)));

    // A queued console byte selects short maintenance, not the guest's later compare.
    f.runtime
        .manager
        .vm(0)
        .unwrap()
        .devices
        .io
        .lock()
        .console
        .access(0, Some(65))
        .unwrap();
    deadline(&f, 1, Some(start + 600));
    IDLE_RETURN.with(|r| r.set(Some((start + 15000, Timer))));
    assert_eq!(idle(&f, &mut local), Some(start + 100));
    causes[Timer as usize] += 1;
    assert_eq!(f.runtime.idle_wakes(), Some((causes, bins)));
}

#[test]
fn mixed_payload_and_smp_keep_the_maintenance_tick() {
    for (harts, linux_peer) in [(1, true), (2, false)] {
        let f = guests(harts, linux_peer);
        let mut local = Dispatcher::new();
        for slot in 0..2 {
            park(&f, slot, &mut local);
            deadline(&f, slot, Some(1900));
        }
        super::model::OWNER.with(|o| o.set(0));
        NOW.with(|n| n.set(1000));
        assert_eq!(idle(&f, &mut local), Some(1100));
    }
}

#[test]
fn firmware_worker_uses_only_local_deadlines_across_stop_and_restart() {
    use super::model::{IDLE_KICK, OWNER};
    let f = guests(2, false);
    let rt = &f.runtime;
    let vm = rt.manager.vm(1).unwrap();
    let mut local = Dispatcher::new();
    park(&f, 0, &mut local);
    park(&f, 1, &mut local);
    NOW.with(|n| n.set(1000));
    deadline(&f, 0, Some(1001)); // Competing manager timer is on another hart.
    deadline(&f, 1, Some(8000));
    assert_eq!(idle(&f, &mut local), Some(8000)); // No 100 us poll or 1 ms cap.
    NOW.with(|n| n.set(8000));
    assert_eq!(idle(&f, &mut local), None); // Do not hide an expired compare in WFI.
    assert!(rt.dispatch(&mut local).unwrap());

    OWNER.with(|o| o.set(0));
    vm.host_stop();
    OWNER.with(|o| o.set(1));
    assert!(idle(&f, &mut local).is_some()); // Still owes retirement.
    assert!(!rt.dispatch(&mut local).unwrap());
    OWNER.with(|o| o.set(0));
    rt.manager.service(rt.board);
    assert!(vm.lifecycle.joined());
    OWNER.with(|o| o.set(1));
    rt.idle(&mut local).unwrap();
    assert_eq!(IDLE_DEADLINE.with(|d| d.get()), None);
    OWNER.with(|o| o.set(0));
    rt.manager.start(rt.board, 1).unwrap();
    OWNER.with(|o| o.set(1));
    assert!(idle(&f, &mut local).is_some()); // Fresh runnable epoch, not the old timer.
    assert!(IDLE_KICK.with(|k| k.get()));
    park(&f, 1, &mut local);
    rt.idle(&mut local).unwrap();
    assert_eq!(IDLE_DEADLINE.with(|d| d.get()), None);
}

#[test]
fn overcommitted_firmware_worker_arms_the_earliest_local_guest() {
    let f = guests_on(2, false, pauli_config::CpuAffinity::Workers, 0);
    let mut local = Dispatcher::new();
    park(&f, 0, &mut local);
    park(&f, 1, &mut local);
    NOW.with(|n| n.set(1000));
    deadline(&f, 0, Some(4500));
    deadline(&f, 1, Some(3200));
    assert_eq!(idle(&f, &mut local), Some(3200));
    deadline(&f, 1, None);
    assert_eq!(idle(&f, &mut local), Some(4500));
}

#[test]
fn pending_console_output_keeps_the_fast_poll_until_drained() {
    let f = guests(1, false);
    let mut local = Dispatcher::new();
    park(&f, 0, &mut local);
    park(&f, 1, &mut local);
    NOW.with(|n| n.set(1000));
    deadline(&f, 1, Some(1800));
    assert_eq!(idle(&f, &mut local), Some(1800));

    let vm = f.runtime.manager.vm(0).unwrap();
    let io = &vm.devices.io;
    io.lock().console.access(0, Some(b'a'.into())).unwrap();
    assert_eq!(idle(&f, &mut local), Some(1100));
    let mut out = [0];
    assert_eq!(io.lock().console.drain(&mut out), 1);
    assert_eq!(&out, b"a");
    assert_eq!(idle(&f, &mut local), Some(1800));
}

#[test]
fn stop_and_cold_restart_cannot_contribute_an_old_timer() {
    let f = guests(1, false);
    let rt = &f.runtime;
    let mut local = Dispatcher::new();
    park(&f, 0, &mut local);
    park(&f, 1, &mut local);
    NOW.with(|n| n.set(1000));
    deadline(&f, 0, Some(1250));
    deadline(&f, 1, Some(1800));
    assert_eq!(idle(&f, &mut local), Some(1250));

    let vm = rt.manager.vm(0).unwrap();
    let old = vm.epoch();
    vm.host_stop();
    assert_eq!(idle(&f, &mut local), Some(1100));
    rt.dispatch(&mut local).unwrap();
    assert!(vm.try_join(1000, rt.manager.link()).unwrap());
    assert_eq!(idle(&f, &mut local), Some(1800)); // Joined: only the peer is live.
    rt.manager.start(rt.board, 0).unwrap();
    assert_ne!(vm.epoch(), old);
    assert_eq!(idle(&f, &mut local), Some(1100)); // Fresh boot vCPU is runnable.
    park(&f, 0, &mut local);
    assert_eq!(idle(&f, &mut local), Some(1800)); // Only the peer still has a timer.
}
