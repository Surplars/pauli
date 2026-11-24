//! Fixed-phase WFI guests competing on one model hart with ordinary CPU-bound guests.
//! Only the real dispatcher owns guest contexts during entry. Each receipt is paired
//! with the absolute compare set by that guest's previous WFI, not its peer's exit.
use super::{Dispatcher, ENTRY, ExitKind, Fixture, QUANTUM, Timer, deadline, guests, park, steps};
use crate::arch::ArchTimer;

const RELEASES: usize = 24;

fn paired<const N: usize>(period: u64, waiters: [usize; 2]) -> [u64; 2] {
    let f = guests::<N>();
    let mut local = Dispatcher::new();
    for slot in waiters {
        let vm = f.runtime.manager.vm(slot).unwrap();
        let mut data = vm.cpu(0).unwrap().data.lock();
        let cpu = data.context.as_mut().unwrap();
        cpu.wfi_timer_period = period;
        cpu.script
            .extend(core::iter::repeat_n(ExitKind::Wait, RELEASES + 2));
    }
    let mut counts = [0; N];
    let mut visits = [0; 2];
    let mut max_late = [0; 2];
    let mut max_wait = [0; N];
    let mut last_end = [0; N];
    let mut min_busy_steps = [u64::MAX; N];
    let mut last_pair = [0; N];
    let mut complete = 0;
    while complete < RELEASES {
        let start = Timer::now();
        assert!(
            start < period * (RELEASES + 1) as u64,
            "lost a timer receipt"
        );
        assert!(f.runtime.dispatch(&mut local).unwrap());
        let end = Timer::now();
        let next = steps::<N>(&f);
        let slot = (0..N).find(|&slot| counts[slot] != next[slot]).unwrap();
        assert_eq!(end - start, (next[slot] - counts[slot]) * ENTRY);
        max_wait[slot] = max_wait[slot].max(start - last_end[slot]);
        last_end[slot] = end;
        if let Some(w) = waiters.iter().position(|&s| s == slot) {
            let visit = visits[w];
            if visit > 0 {
                let due = period * visit as u64;
                max_late[w] = max_late[w].max(start.checked_sub(due).expect("early admission"));
            }
            visits[w] += 1; // Boot programs the first compare; it is not a receipt.
            if visits[0].min(visits[1]) > complete + 1 {
                for b in (0..N).filter(|b| !waiters.contains(b)) {
                    min_busy_steps[b] = min_busy_steps[b].min(next[b] - last_pair[b]);
                }
                last_pair = next;
                complete += 1;
            }
        }
        counts = next;
    }
    for b in (0..N).filter(|b| !waiters.contains(b)) {
        max_wait[b] = max_wait[b].max(Timer::now() - last_end[b]);
        assert!(
            min_busy_steps[b] > 0,
            "no service between two complete release pairs: {period} {waiters:?} {b}"
        );
        assert!(max_wait[b] < period, "stalled busy slot {b}: {max_wait:?}");
    }
    std::println!(
        "period={period} waiters={waiters:?} x{RELEASES}: worst_late_us={max_late:?} steps={counts:?}"
    );
    max_late
}

#[test]
fn coincident_periodic_waiters_on_a_shared_hart() {
    // Sweep harmonic and off-grid guest periods, including every pair across
    // the placement group boundary. An ISA entry cannot be split mid-step.
    for period in [2_000, 2_100, 1_700, 2_350] {
        for i in 0..5 {
            for j in i + 1..5 {
                let late = paired::<5>(period, [i, j]);
                assert!(
                    late.into_iter().all(|ticks| ticks <= 2 * ENTRY),
                    "{period} us, [{i},{j}]: {late:?}"
                );
            }
        }
    }
}

fn first_expiry(second: u64) -> (Fixture, Dispatcher<Timer>) {
    let f = guests::<4>();
    let mut local = Dispatcher::new();
    for (slot, at) in [3 * ENTRY, second].into_iter().enumerate() {
        deadline(&f, slot, Some(at));
        park(&f, slot, &mut local);
    }
    assert!(f.runtime.dispatch(&mut local).unwrap()); // VM2 yields at 300 us.
    assert_eq!(steps::<4>(&f), [1, 1, 1, 0]);
    (f, local)
}

fn short_handler(f: &Fixture, slot: usize) {
    // The guest consumes the original compare before its next WFI. Do not
    // rewrite the live compare of the currently parked guest from the harness.
    let vm = f.runtime.manager.vm(slot).unwrap();
    let mut data = vm.cpu(0).unwrap().data.lock();
    let guest = data.context.as_mut().unwrap();
    guest.wfi_timer_interval = Some(2_000);
    guest.script.push_back(ExitKind::Wait);
}

#[test]
fn cancelling_the_second_waiter_does_not_delay_the_regular_cursor() {
    for fault in [false, true] {
        let (f, mut local) = first_expiry(3 * ENTRY);
        let rt = &f.runtime;
        short_handler(&f, 0);
        assert!(rt.dispatch(&mut local).unwrap());
        let peer = rt.manager.vm(1).unwrap();
        if fault {
            peer.request_stop(true, false);
        } else {
            peer.host_stop();
        }
        assert!(rt.dispatch(&mut local).unwrap());
        assert_eq!(steps::<4>(&f), [2, 1, 1, 3]);
        assert!(peer.try_join(Timer::now(), rt.manager.link()).unwrap());
        let old = peer.epoch();
        rt.manager.start(rt.board, 1).unwrap();
        assert_ne!(peer.epoch(), old);
        assert!(rt.dispatch(&mut local).unwrap());
        assert_eq!(steps::<4>(&f), [2, 3, 1, 3]); // Fresh VM1 is next in RR.
    }
}

#[test]
fn a_full_or_still_pending_handler_cannot_extend_the_timer_burst() {
    for repark_due in [false, true] {
        let (f, mut local) = first_expiry(3 * ENTRY);
        let rt = &f.runtime;
        if repark_due {
            park(&f, 0, &mut local); // WFI without clearing the timer's pending level.
        } else {
            assert!(rt.dispatch(&mut local).unwrap()); // Full 300 us spent by VM0.
        }
        let before = steps::<4>(&f);
        assert_eq!(before, [if repark_due { 2 } else { 4 }, 1, 1, 0]);
        assert!(rt.dispatch(&mut local).unwrap());
        assert_eq!(
            steps::<4>(&f),
            [before[0], 1, 1, 3],
            "VM3 owns the next ordinary turn"
        );
    }
}

#[test]
fn only_the_original_compare_may_spend_the_remaining_budget() {
    for coincident in [false, true] {
        let (f, mut local) = first_expiry(3 * ENTRY + if coincident { 0 } else { ENTRY / 2 });
        short_handler(&f, 0);
        let rt = &f.runtime;
        assert!(rt.dispatch(&mut local).unwrap());
        assert_eq!(Timer::now(), 4 * ENTRY);
        assert!(rt.dispatch(&mut local).unwrap());
        if coincident {
            // The CPU-bound second handler gets only the unspent 200 us.
            assert_eq!(steps::<4>(&f), [2, 3, 1, 0]);
            assert!(rt.dispatch(&mut local).unwrap());
            assert_eq!(steps::<4>(&f), [2, 3, 1, 3]);
        } else {
            // VM1's 350-us compare is due now, but did not trigger this handoff.
            assert_eq!(steps::<4>(&f), [2, 1, 1, 3]);
        }
    }
}

#[test]
fn four_short_handlers_share_one_urgent_budget_before_a_regular_peer_runs() {
    let f = guests::<6>();
    let rt = &f.runtime;
    let mut local = Dispatcher::new();
    const DUE: u64 = 5 * ENTRY;
    for w in 0..4 {
        deadline(&f, w, Some(DUE));
        park(&f, w, &mut local);
        short_handler(&f, w);
    }
    assert!(rt.dispatch(&mut local).unwrap()); // Busy VM4 reaches all four compares.
    assert_eq!(Timer::now(), DUE);
    for _ in 0..3 {
        assert!(rt.dispatch(&mut local).unwrap()); // Three handlers can finish within Q.
    }
    let before = steps::<6>(&f);
    assert_eq!(Timer::now(), DUE + QUANTUM);
    assert_eq!(
        before[..4].iter().filter(|&&s| s == 2).count(),
        3,
        "{before:?}"
    );
    assert_eq!(before[4..], [1, 0]);
    assert!(rt.dispatch(&mut local).unwrap()); // No fourth bonus admission ahead of VM5.
    assert_eq!(steps::<6>(&f)[5], QUANTUM / ENTRY);
}

// Same expiry, quanta W0=900, W1=200, W2=900. VM3 trips the compare; VM4 is
// next in ordinary rotation. Use real VM configurations, including exact clones.
fn unequal_quanta() -> Fixture {
    let f = Fixture::with_config(1, [1; 2], 1, |slot, vm| {
        vm.quantum_us = if slot == 0 { 900 } else { 200 };
        vm.memory_size = 32 * 1024;
        vm.firmware = Some(pauli_config::FirmwareLayout {
            rx_pages: 1,
            ro_pages: 0,
        });
    });
    let rt = &f.runtime;
    for (template, name) in [(0, "long"), (1, "busyA"), (1, "busyB")] {
        rt.manager
            .provision(
                rt.board,
                None,
                template,
                pauli_control::ChildName::new(name).unwrap().into(),
            )
            .unwrap();
    }
    for slot in 0..5 {
        rt.manager.start(rt.board, slot).unwrap();
    }
    f
}

#[test]
fn a_shorter_middle_quantum_limits_its_owner_not_the_original_burst() {
    for spend_full_middle in [false, true] {
        let f = unequal_quanta();
        let rt = &f.runtime;
        let mut local = Dispatcher::new();
        for w in 0..3 {
            deadline(&f, w, Some(4 * ENTRY));
            park(&f, w, &mut local);
        }
        assert!(rt.dispatch(&mut local).unwrap()); // VM3 reaches t=400.
        short_handler(&f, 0);
        if spend_full_middle {
            let vm = rt.manager.vm(1).unwrap();
            vm.cpu(0)
                .unwrap()
                .data
                .lock()
                .context
                .as_mut()
                .unwrap()
                .script
                .push_back(ExitKind::Resume);
        }
        short_handler(&f, 1);
        for _ in 0..3 {
            assert!(rt.dispatch(&mut local).unwrap());
        }
        if spend_full_middle {
            // VM1 used its entire 200 us even though it parked on its last entry.
            assert_eq!(steps::<5>(&f), [2, 3, 1, 1, 2]);
        } else {
            // VM1 used just 100 us. VM2 may use the remaining 700 us of VM0's
            // original 900-us budget, not the 100 us left of VM1's own quantum.
            assert_eq!(steps::<5>(&f), [2, 2, 8, 1, 0]);
            assert_eq!(Timer::now(), 1_300);
            assert!(rt.dispatch(&mut local).unwrap());
            assert_eq!(steps::<5>(&f)[4], 2);
        }
    }
}
