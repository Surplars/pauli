//! Two pinned dispatchers on one deterministic model clock, driven in sequence.
//! Observe each real admission, native WFI request and VM incarnation; simulated
//! execution is not parallel and cannot establish a wall-clock latency bound.
use super::super::model::{IDLE_DEADLINE, IDLE_ENTRIES, KICKED_OWNERS, OWNER};
use super::{Dispatcher, ExitKind, Fixture, NOW, Timer, deadline, guests_on, idle, park};
use crate::arch::ArchTimer;
use pauli_config::{CpuAffinity, FirmwareLayout};
use pauli_control::ChildName;

fn fixture(timer_owner: usize) -> Fixture {
    let f = Fixture::with_config(2, [1, 1], 1, |slot, vm| {
        let mut pins = [0; pauli_config::MAX_GUEST_VCPUS as usize];
        pins[0] = (if slot == 0 {
            timer_owner
        } else {
            1 - timer_owner
        }) as u8;
        vm.cpu_affinity = CpuAffinity::Physical(pins);
        vm.firmware = Some(FirmwareLayout {
            rx_pages: 1,
            ro_pages: 0,
        });
        vm.memory_size = 32 * 1024;
    });
    let rt = &f.runtime;
    for name in ["busy2", "busy3"] {
        rt.manager
            .provision(rt.board, None, 1, ChildName::new(name).unwrap().into())
            .unwrap();
    }
    for slot in 0..4 {
        rt.manager.start(rt.board, slot).unwrap();
        let owner = rt.manager.vm(slot).unwrap().cpu(0).unwrap().owner;
        assert_eq!(
            owner,
            if slot == 0 {
                timer_owner
            } else {
                1 - timer_owner
            }
        );
    }
    f
}

#[test]
fn own_timer_bounds_idle_despite_a_busy_remote_hart() {
    for (timer_owner, far) in [(1, 1_000), (0, 300)] {
        let f = fixture(timer_owner);
        let mut local = Dispatcher::new();
        deadline(&f, 0, Some(1_000));
        park(&f, 0, &mut local);
        assert_eq!(Timer::now(), 100);
        // Workers need no maintenance poll; the boot hart still services remote
        // devices. In both cases an earlier local expiry wins.
        for (now, expected) in [(200, Some(far)), (970, Some(1_000)), (1_000, None)] {
            // TIME can cross the compare between the empty pass and the masked WFI.
            NOW.with(|t| t.set(now.min(999)));
            assert!(!f.runtime.dispatch(&mut local).unwrap());
            NOW.with(|t| t.set(now));
            assert_eq!(
                idle(&f, &mut local),
                expected,
                "owner={timer_owner}, now={now}"
            );
        }
    }
}

#[test]
fn a_remote_due_compare_and_pending_retirement_do_not_shorten_monitor_maintenance() {
    // No vCPU is placed on the boot hart. The remote firmware really returns
    // from VS via WFI: do not change a parked context behind its publication.
    let f = guests_on(2, false, CpuAffinity::Workers, 0);
    let (mut monitor, mut worker) = (Dispatcher::new(), Dispatcher::new());
    deadline(&f, 0, Some(225));
    park(&f, 0, &mut worker);
    park(&f, 1, &mut worker); // Permanently disarmed.
    assert_eq!(Timer::now(), 200);
    assert_eq!(idle(&f, &mut worker), Some(225));
    OWNER.with(|o| o.set(0));
    assert_eq!(idle(&f, &mut monitor), Some(300));
    NOW.with(|t| t.set(225));
    assert_eq!(idle(&f, &mut monitor), Some(325)); // Remote expiry, not local work.
    let vm = f.runtime.manager.vm(0).unwrap();
    vm.host_stop();
    assert_eq!(idle(&f, &mut monitor), Some(325));
    OWNER.with(|o| o.set(1));
    assert!(!f.runtime.dispatch(&mut worker).unwrap()); // Only retirement, no guest entry.
    OWNER.with(|o| o.set(0));
    assert!(vm.try_join(Timer::now(), f.runtime.manager.link()).unwrap());
    assert_eq!(idle(&f, &mut monitor), Some(325));
    OWNER.with(|o| o.set(1));
    IDLE_ENTRIES.with(|n| n.set(0));
    f.runtime.idle(&mut worker).unwrap();
    assert_eq!(IDLE_ENTRIES.with(|n| n.get()), 1);
    assert_eq!(IDLE_DEADLINE.with(|t| t.get()), None); // Only untimed worker WFI remains.
}

#[test]
fn monitor_uses_the_earliest_local_compare_across_placement_groups() {
    let f = fixture(0);
    let rt = &f.runtime;
    let slot = rt
        .manager
        .provision(rt.board, None, 0, ChildName::new("timer2").unwrap().into())
        .unwrap();
    assert_eq!(slot, crate::placement::GROUP);
    rt.manager.start(rt.board, slot).unwrap();
    let (mut monitor, mut worker) = (Dispatcher::new(), Dispatcher::new());
    for (slot, at) in [(0, 1_000), (slot, 700)] {
        deadline(&f, slot, Some(at));
        park(&f, slot, &mut monitor);
    }
    deadline(&f, 1, Some(650));
    park(&f, 1, &mut worker);
    OWNER.with(|o| o.set(0));
    NOW.with(|t| t.set(630));
    assert!(!rt.dispatch(&mut monitor).unwrap());
    assert_eq!(idle(&f, &mut monitor), Some(700)); // Not the remote 650 or the poll at 730.
}

fn counts(f: &Fixture) -> [u64; 4] {
    core::array::from_fn(|slot| {
        let vm = f.runtime.manager.vm(slot).unwrap();
        let data = vm.cpu(0).unwrap().data.lock();
        data.context.as_ref().unwrap().steps
    })
}

#[test]
fn periodic_releases_keep_their_owner_through_remote_overcommit_stop_and_cold_start() {
    for timer_owner in [1, 0] {
        let f = fixture(timer_owner);
        let rt = &f.runtime;
        let worker_owner = 1 - timer_owner;
        let (mut timer, mut worker) = (Dispatcher::new(), Dispatcher::new());
        let control = rt.manager.vm(0).unwrap();
        let clone = rt.manager.vm(2).unwrap();
        let epochs: [u32; 4] = core::array::from_fn(|s| rt.manager.vm(s).unwrap().epoch());
        const PERIOD: u64 = 1_000; // One 100-tick timed entry and three 300-tick busy grants.
        {
            let mut data = control.cpu(0).unwrap().data.lock();
            let ctx = data.context.as_mut().unwrap();
            ctx.wfi_timer_period = PERIOD;
            ctx.script.extend([ExitKind::Wait; 13]);
        }
        OWNER.with(|o| o.set(timer_owner));
        assert!(rt.dispatch(&mut timer).unwrap()); // Boot programs its first compare.
        for release in 1..=12 {
            // As in the actual dispatch/idle loop, an empty local pass precedes
            // each WFI decision. The other modeled owner advances TIME between them.
            OWNER.with(|o| o.set(timer_owner));
            assert!(!rt.dispatch(&mut timer).unwrap());
            OWNER.with(|o| o.set(0)); // Only management changes lifecycle or rebuilds the epoch.
            if release == 4 {
                KICKED_OWNERS.with(|k| k.set(0));
                clone.host_stop();
                assert_eq!(KICKED_OWNERS.with(|k| k.get()), 1 << worker_owner);
                assert!(!clone.try_join(Timer::now(), rt.manager.link()).unwrap());
            } else if release == 7 {
                rt.manager.start(rt.board, 2).unwrap();
                assert_ne!(clone.epoch(), epochs[2]);
                assert_eq!(counts(&f)[2], 0);
                assert_eq!(clone.cpu(0).unwrap().owner, worker_owner);
            }
            let before = counts(&f);
            OWNER.with(|o| o.set(worker_owner));
            for _ in 0..3 {
                assert!(rt.dispatch(&mut worker).unwrap());
            }
            let after = counts(&f);
            assert_eq!(after[0], before[0], "remote dispatch entered timer VM");
            assert!(
                after[1] > before[1] && after[3] > before[3],
                "remote sibling stalled"
            );
            assert_eq!(after[2] > before[2], !(4..7).contains(&release));
            if release == 4 {
                OWNER.with(|o| o.set(0));
                assert!(clone.try_join(Timer::now(), rt.manager.link()).unwrap());
            }
            OWNER.with(|o| o.set(timer_owner));
            assert_eq!(Timer::now(), PERIOD * release);
            assert_eq!(
                idle(&f, &mut timer),
                None,
                "local expiry is ready for dispatch"
            );
            assert!(rt.dispatch(&mut timer).unwrap());
            assert_eq!(counts(&f), [release + 1, after[1], after[2], after[3]]);
        }
        for slot in [0, 1, 3] {
            assert_eq!(rt.manager.vm(slot).unwrap().epoch(), epochs[slot]);
        }
    }
}
