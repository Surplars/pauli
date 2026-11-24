//! Same-hart slice bounds at the guest-entry boundary, with a 1 MHz clock.
//! A mock guest entry takes 100 us; only real dispatcher decisions advance time.
use super::super::model::OWNER;
use super::{Dispatcher, Fixture, Timer, deadline, guests, guests_on, park};
use crate::arch::{ArchTimer, Mailbox};

fn steps(f: &Fixture, slot: usize) -> u64 {
    let vm = f.runtime.manager.vm(slot).unwrap();
    vm.cpu(0)
        .unwrap()
        .data
        .lock()
        .context
        .as_ref()
        .unwrap()
        .steps
}

#[test]
fn busy_peer_releases_the_hart_at_the_waiters_compare() {
    let f = guests(1, false);
    let rt = &f.runtime;
    let mut local = Dispatcher::new();
    deadline(&f, 0, Some(500));
    park(&f, 0, &mut local);
    assert_eq!(Timer::now(), 100);

    // The 300-us quantum wins first. Then the 500-us compare must win
    // over a fresh quantum ending at 700, without an early waiter entry.
    for (end, peer_steps) in [(400, 3), (500, 4)] {
        assert!(rt.dispatch(&mut local).unwrap());
        assert_eq!(
            (Timer::now(), steps(&f, 0), steps(&f, 1)),
            (end, 1, peer_steps)
        );
    }
    assert!(rt.dispatch(&mut local).unwrap());
    assert!(steps(&f, 0) > 1);
}

#[test]
fn busy_slice_uses_the_earliest_compare_across_placement_groups() {
    use pauli_control::ChildName;
    for (first, last) in [(700, 900), (900, 700)] {
        let f = guests_on(1, false, pauli_config::CpuAffinity::Automatic, 1);
        let rt = &f.runtime;
        // Leave two joined children in the directory; only the final child
        // runs, alongside the two boot VMs, in the second placement group.
        let mut child = 0;
        for name in ["spareA", "spareB", "waiter"] {
            child = rt
                .manager
                .provision(rt.board, None, 1, ChildName::new(name).unwrap().into())
                .unwrap();
        }
        assert_eq!(child, crate::placement::GROUP);
        rt.manager.start(rt.board, child).unwrap();
        let mut local = Dispatcher::new();
        deadline(&f, 0, Some(first));
        park(&f, 0, &mut local); // 0..100
        assert!(rt.dispatch(&mut local).unwrap()); // Busy VM: 100..400
        deadline(&f, child, Some(last));
        park(&f, child, &mut local); // 400..500

        assert!(rt.dispatch(&mut local).unwrap());
        assert_eq!((Timer::now(), steps(&f, 0), steps(&f, child)), (700, 1, 1));
        assert!(rt.dispatch(&mut local).unwrap());
        let (due, future) = if first < last { (0, child) } else { (child, 0) };
        assert!(steps(&f, due) > 1);
        assert_eq!(steps(&f, future), 1);
    }
}

#[test]
fn a_remote_waiter_does_not_shorten_the_local_quantum() {
    let f = guests(2, false);
    let mut remote = Dispatcher::new();
    deadline(&f, 0, Some(200));
    park(&f, 0, &mut remote);
    let peer_owner = f.runtime.manager.vm(1).unwrap().cpu(0).unwrap().owner;
    OWNER.with(|owner| assert_ne!(owner.replace(peer_owner), peer_owner));
    assert!(f.runtime.dispatch(&mut Dispatcher::new()).unwrap());
    assert_eq!((Timer::now(), steps(&f, 0), steps(&f, 1)), (400, 1, 3));
}

#[test]
fn a_mailbox_wake_removes_the_waiters_old_slice_bound() {
    let f = guests(1, false);
    let rt = &f.runtime;
    let mut local = Dispatcher::new();
    deadline(&f, 0, Some(200));
    park(&f, 0, &mut local);
    let vm = rt.manager.vm(0).unwrap();
    let cpu = vm.cpu(0).unwrap();
    cpu.mailbox.lock().post_ipi();

    // The pre-entry mail scan woke the waiter before its compare. With
    // both peers now runnable, ordinary round-robin retains its full slice.
    assert!(rt.dispatch(&mut local).unwrap());
    assert_eq!(cpu.data.lock().context.as_ref().unwrap().ipis, 1);
    assert_eq!((Timer::now(), steps(&f, 0), steps(&f, 1)), (400, 1, 3));
}

#[test]
fn stopped_and_restarted_waiters_do_not_leave_a_slice_bound() {
    // 200 would cut the peer's slice during stop; 700 would cut its slice
    // after the replacement parks without programming its own timer.
    for old_compare in [200, 700] {
        let f = guests(1, false);
        let rt = &f.runtime;
        let mut local = Dispatcher::new();
        deadline(&f, 0, Some(old_compare));
        park(&f, 0, &mut local);
        let vm = rt.manager.vm(0).unwrap();
        let epoch = vm.epoch();
        vm.host_stop();
        assert!(rt.dispatch(&mut local).unwrap());
        assert_eq!(Timer::now(), 400);
        assert!(vm.try_join(Timer::now(), rt.manager.link()).unwrap());
        rt.manager.start(rt.board, 0).unwrap();
        assert_ne!(epoch, vm.epoch());
        park(&f, 0, &mut local);
        assert!(rt.dispatch(&mut local).unwrap());
        assert_eq!((Timer::now(), steps(&f, 0)), (800, 1));
        assert!(rt.dispatch(&mut local).unwrap());
        assert_eq!(steps(&f, 0), 1, "old compare woke a new incarnation");
    }
}

#[test]
fn an_expired_compare_does_not_clip_the_woken_vcpus_own_slice() {
    let f = guests(1, false);
    let mut local = Dispatcher::new();
    deadline(&f, 0, Some(200));
    park(&f, 0, &mut local);
    park(&f, 1, &mut local);
    assert_eq!(Timer::now(), 200);
    assert!(f.runtime.dispatch(&mut local).unwrap());
    assert_eq!((Timer::now(), steps(&f, 0), steps(&f, 1)), (500, 4, 1));
}

#[test]
fn externally_woken_vcpu_uses_the_next_peers_compare_not_its_own() {
    let f = guests(1, false);
    let mut local = Dispatcher::new();
    deadline(&f, 0, Some(300));
    deadline(&f, 1, Some(400));
    park(&f, 0, &mut local);
    park(&f, 1, &mut local);
    // The external line wakes VM0 at 200, with both compares still ahead.
    f.runtime.manager.vm(0).unwrap().devices.irqs.lock().lines[0] = true;
    assert!(f.runtime.dispatch(&mut local).unwrap());
    assert_eq!((Timer::now(), steps(&f, 0), steps(&f, 1)), (400, 3, 1));
}

fn waiter_turn(f: &Fixture, local: &mut Dispatcher<Timer>, lead: Option<u64>) {
    let vm = f.runtime.manager.vm(0).unwrap();
    {
        let mut data = vm.cpu(0).unwrap().data.lock();
        let guest = data.context.as_mut().unwrap();
        guest.wfi_timer_interval = lead;
    }
    park(f, 0, local);
}

fn busy_turn(f: &Fixture, local: &mut Dispatcher<Timer>) -> u64 {
    let before = steps(f, 1);
    assert!(f.runtime.dispatch(local).unwrap());
    steps(f, 1) - before
}

#[test]
fn rearming_waiter_and_busy_peer_progress_with_only_the_unused_quantum_protected() {
    for (lead, entries) in [
        (0, [3, 3, 3, 3, 3, 3]),   // Already due: plain round-robin.
        (50, [1, 2, 1, 2, 1, 2]),  // 100 us actually spent, not 50 us.
        (100, [1, 2, 1, 2, 1, 2]), // 200 us owed, not a fresh 300-us quantum.
        (200, [2, 2, 2, 2, 2, 2]), // A later compare can exceed the 100-us floor.
    ] {
        let f = guests(1, false);
        let mut local = Dispatcher::new();
        for (turn, expected) in entries.into_iter().enumerate() {
            waiter_turn(&f, &mut local, Some(lead));
            assert_eq!(steps(&f, 0), turn as u64 + 1);
            assert_eq!(
                busy_turn(&f, &mut local),
                expected,
                "lead {lead}, turn {turn}"
            );
        }
    }
}

#[test]
fn repayment_with_no_near_compare_still_allows_a_full_quantum() {
    for lead in [None, Some(500)] {
        let f = guests(1, false);
        let mut local = Dispatcher::new();
        waiter_turn(&f, &mut local, Some(100));
        assert_eq!(busy_turn(&f, &mut local), 1);
        waiter_turn(&f, &mut local, lead);
        assert_eq!(busy_turn(&f, &mut local), 3);
    }
}

#[test]
fn native_exit_carries_only_outstanding_repayment() {
    let f = guests(1, false);
    let mut local = Dispatcher::new();
    waiter_turn(&f, &mut local, Some(100));
    assert_eq!(busy_turn(&f, &mut local), 1); // 200 us owed.

    let vm = f.runtime.manager.vm(1).unwrap();
    vm.cpu(0)
        .unwrap()
        .data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .native_event = true;
    for expected in [1, 1, 1, 2] {
        // The native exit consumes 100 us; next pay the remaining 100.
        // Only the *following* fresh cut can mint another 200-us repayment.
        waiter_turn(&f, &mut local, Some(100));
        assert_eq!(busy_turn(&f, &mut local), expected);
    }
}

#[test]
fn parking_or_cold_restart_releases_the_busy_guests_credit() {
    for restart in [false, true] {
        let f = guests(1, false);
        let mut local = Dispatcher::new();
        waiter_turn(&f, &mut local, Some(100));
        assert_eq!(busy_turn(&f, &mut local), 1);
        let vm = f.runtime.manager.vm(1).unwrap();
        if restart {
            let epoch = vm.epoch();
            vm.host_stop();
            waiter_turn(&f, &mut local, Some(100)); // Scan retires the old busy guest.
            assert!(vm.try_join(Timer::now(), f.runtime.manager.link()).unwrap());
            f.runtime.manager.start(f.runtime.board, 1).unwrap();
            assert_ne!(vm.epoch(), epoch);
        } else {
            waiter_turn(&f, &mut local, Some(100));
            deadline(&f, 1, Some(Timer::now() + 100));
            park(&f, 1, &mut local); // WFI after only half of its 200-us repayment.
            waiter_turn(&f, &mut local, Some(100));
        }
        assert_eq!(busy_turn(&f, &mut local), 1);
        waiter_turn(&f, &mut local, Some(100));
        assert_eq!(
            busy_turn(&f, &mut local),
            2,
            "first admission must be fresh"
        );
    }
}
