//! Single physical owner, independent single-vCPU guests and a 1 MHz mock clock.
//! Observe only guest entries and time at dispatch boundaries, never cursor hints.
use super::{Dispatcher, ExitKind, Fixture, Timer, deadline, guests_on, park};
use crate::arch::ArchTimer;
use pauli_config::CpuAffinity;
use pauli_control::ChildName;
mod cohort;

const ENTRY: u64 = 100;
const QUANTUM: u64 = 300;
const TURNS: usize = 120;

fn guests<const N: usize>() -> Fixture {
    let f = guests_on(1, false, CpuAffinity::Automatic, 1);
    let rt = &f.runtime;
    for (i, name) in ["busyC", "busyD", "busyE", "busyF"]
        .iter()
        .take(N - 2)
        .enumerate()
    {
        let slot = rt
            .manager
            .provision(rt.board, None, 1, ChildName::new(name).unwrap().into())
            .unwrap();
        assert_eq!(slot, i + 2);
        rt.manager.start(rt.board, slot).unwrap();
    }
    f
}

fn steps<const N: usize>(f: &Fixture) -> [u64; N] {
    core::array::from_fn(|slot| {
        let vm = f.runtime.manager.vm(slot).unwrap();
        vm.cpu(0)
            .unwrap()
            .data
            .lock()
            .context
            .as_ref()
            .unwrap()
            .steps
    })
}

#[derive(Debug)]
struct Sample<const N: usize> {
    service: [u64; N],
    visits: [usize; N],
    max_wait: [u64; N], // Previous end of service (or boot) -> next admission.
    max_late: u64,      // Guest's last WFI compare -> next admission.
}

fn sample<const N: usize>(timers: &[(usize, u64)]) -> Sample<N> {
    let f = guests::<N>();
    let mut local = Dispatcher::new();
    for &(slot, lead) in timers {
        let vm = f.runtime.manager.vm(slot).unwrap();
        let mut data = vm.cpu(0).unwrap().data.lock();
        let ctx = data.context.as_mut().unwrap();
        ctx.wfi_timer_interval = Some(lead);
        ctx.script
            .extend(core::iter::repeat_n(ExitKind::Wait, TURNS));
    }
    let mut counts = [0; N];
    let mut last_end = [0; N];
    let mut result = Sample {
        service: [0; N],
        visits: [0; N],
        max_wait: [0; N],
        max_late: 0,
    };
    for _ in 0..TURNS {
        let start = Timer::now();
        assert!(f.runtime.dispatch(&mut local).unwrap());
        let end = Timer::now();
        let next = steps::<N>(&f);
        let slot = (0..N).find(|&slot| counts[slot] != next[slot]).unwrap();
        let elapsed = (next[slot] - counts[slot]) * ENTRY;
        assert_eq!(
            end - start,
            elapsed,
            "one dispatch must serve exactly one guest"
        );
        let wait = start - last_end[slot];
        if result.visits[slot] != 0
            && let Some(&(_, lead)) = timers.iter().find(|&&(waiter, _)| waiter == slot)
        {
            assert!(wait >= lead, "guest entered before compare");
            result.max_late = result.max_late.max(wait - lead);
        }
        result.service[slot] += elapsed;
        result.visits[slot] += 1;
        result.max_wait[slot] = result.max_wait[slot].max(wait);
        last_end[slot] = end;
        counts = next;
    }
    // A lane skipped until the end of the sample has not returned to close
    // its last interval. Count that lower bound instead of hiding starvation.
    for (longest, last) in result.max_wait.iter_mut().zip(last_end) {
        *longest = (*longest).max(Timer::now() - last);
    }
    std::println!("lanes={N} timers={timers:?} (slot, lead_us); {result:?} (us)");
    result
}

#[test]
fn all_busy_peers_rotate_even_across_placement_groups() {
    // Five guests cross the four-VM directory group; all get a full slice.
    let s = sample::<5>(&[]);
    assert_eq!(s.visits, [TURNS / 5; 5]);
    assert_eq!(s.service, [(TURNS as u64 / 5) * QUANTUM; 5]);
    assert_eq!(s.max_wait, [4 * QUANTUM; 5]);
}

#[test]
fn a_future_compare_does_not_wait_behind_an_unrelated_busy_quantum() {
    // Permute the sleeper: a favorable directory slot must not hide the delay.
    for waiter in 0..3 {
        let s = sample::<3>(&[(waiter, ENTRY)]);
        assert!(s.max_late <= ENTRY, "waiter={waiter}: {s:?}");
        let [a, b] = [(waiter + 1) % 3, (waiter + 2) % 3];
        assert!(s.visits.iter().all(|&v| v >= TURNS / 5), "{s:?}");
        assert!(s.service[a].abs_diff(s.service[b]) <= QUANTUM, "{s:?}");
        assert!(s.max_wait[a].max(s.max_wait[b]) <= 2 * QUANTUM, "{s:?}");
    }
}

#[test]
fn a_timer_across_the_group_boundary_keeps_all_four_busy_peers_moving() {
    let s = sample::<5>(&[(4, ENTRY)]);
    assert!(s.max_late <= ENTRY, "{s:?}");
    let peers = &s.service[..4];
    assert!(*peers.iter().min().unwrap() != 0, "{s:?}");
    assert!(
        peers.iter().max().unwrap() - peers.iter().min().unwrap() <= QUANTUM,
        "{s:?}"
    );
    assert!(
        s.max_wait[..4].iter().all(|&w| w <= 4 * (QUANTUM + ENTRY)),
        "{s:?}"
    );
}

#[test]
fn immediately_rearmed_waiter_cannot_monopolize_one_hart() {
    let s = sample::<3>(&[(0, 0)]);
    assert!(s.visits.iter().all(|&v| v >= TURNS / 5), "{s:?}");
    assert!(s.max_wait[1].max(s.max_wait[2]) <= 2 * QUANTUM, "{s:?}");
    assert!(s.service[1].abs_diff(s.service[2]) <= QUANTUM, "{s:?}");
}

#[test]
fn two_competing_timers_cannot_chain_preference_ahead_of_regular_lanes() {
    // In the first pair both initial compares coincide at 300 us; in the
    // second both guests continuously expose already-expired timer levels.
    for leads in [(2 * ENTRY, ENTRY), (0, 0)] {
        let s = sample::<4>(&[(0, leads.0), (1, leads.1)]);
        assert!(s.visits.iter().all(|&v| v >= TURNS / 10), "{s:?}");
        assert!(
            s.max_wait.iter().all(|&w| w <= 4 * (QUANTUM + ENTRY)),
            "{s:?}"
        );
    }
}

fn after_timer_service<const N: usize>(stop_peer: bool) {
    let f = guests::<N>();
    let mut local = Dispatcher::new();
    let rt = &f.runtime;
    assert!(rt.dispatch(&mut local).unwrap()); // VM0 and VM1 pass the cold-boot cursor.
    assert!(rt.dispatch(&mut local).unwrap());
    for slot in 2..N {
        park(&f, slot, &mut local);
    }
    deadline(&f, 0, Some(Timer::now() + 2 * ENTRY));
    park(&f, 0, &mut local);
    assert!(rt.dispatch(&mut local).unwrap()); // VM1 reaches VM0's compare.
    let before = steps::<N>(&f);
    assert!(rt.dispatch(&mut local).unwrap()); // VM0 wakes and continues computing.
    assert_eq!(steps::<N>(&f)[0], before[0] + QUANTUM / ENTRY);
    if stop_peer {
        rt.manager.vm(1).unwrap().host_stop();
    }
    assert!(rt.dispatch(&mut local).unwrap());
    let next = steps::<N>(&f);
    if stop_peer {
        assert_eq!(
            next[0],
            before[0] + 2 * QUANTUM / ENTRY,
            "last runnable lane stalled"
        );
        assert_eq!(next[1], before[1], "stopped peer ran");
    } else {
        assert_eq!(next[0], before[0] + QUANTUM / ENTRY, "VM0 got two turns");
        assert_eq!(next[1], before[1] + QUANTUM / ENTRY, "VM1 did not resume");
    }
}

#[test]
fn a_timer_already_next_among_runnable_guests_consumes_its_regular_turn() {
    // Other placed lanes park indefinitely, even across the directory group
    // boundary. Only two lanes can run: an expiry must not give VM0 two quanta.
    after_timer_service::<3>(false);
    after_timer_service::<5>(false);
}

#[test]
fn a_serviced_timer_is_not_skipped_if_every_other_peer_has_stopped_or_parked() {
    after_timer_service::<5>(true);
}

#[test]
fn a_replacement_does_not_inherit_a_served_timer_turn() {
    for exit in [ExitKind::Shutdown, ExitKind::Fault] {
        let f = super::guests(1, false);
        let rt = &f.runtime;
        let mut local = Dispatcher::new();
        deadline(&f, 0, Some(2 * ENTRY));
        park(&f, 0, &mut local);
        assert!(rt.dispatch(&mut local).unwrap()); // Last regular admission: VM1.
        let vm = rt.manager.vm(0).unwrap();
        let epoch = vm.epoch();
        vm.cpu(0)
            .unwrap()
            .data
            .lock()
            .context
            .as_mut()
            .unwrap()
            .script
            .push_back(exit);
        assert!(rt.dispatch(&mut local).unwrap()); // VM0 retires inside its timer visit.
        assert!(vm.try_join(Timer::now(), rt.manager.link()).unwrap());
        rt.manager.start(rt.board, 0).unwrap();
        assert_ne!(vm.epoch(), epoch);
        assert!(rt.dispatch(&mut local).unwrap());
        assert_eq!(
            steps::<2>(&f),
            [QUANTUM / ENTRY, 1],
            "fresh VM0 is next in RR"
        );
    }
}

#[test]
fn stop_or_fault_before_delivery_does_not_advance_the_replacement_or_skip_a_peer() {
    for fault in [false, true] {
        let f = guests::<3>();
        let rt = &f.runtime;
        let mut local = Dispatcher::new();
        deadline(&f, 0, Some(200));
        park(&f, 0, &mut local);
        assert!(rt.dispatch(&mut local).unwrap());
        assert_eq!((Timer::now(), steps::<3>(&f)), (200, [1, 1, 0]));

        // The waiter's compare has elapsed, but the current epoch closes admission
        // before the dispatcher can deliver it. VM2 still owns the next regular turn.
        let vm = rt.manager.vm(0).unwrap();
        let epoch = vm.epoch();
        if fault {
            vm.request_stop(true, false);
        } else {
            vm.host_stop();
        }
        assert!(rt.dispatch(&mut local).unwrap());
        assert_eq!(steps::<3>(&f), [1, 1, 3]);
        assert!(vm.try_join(Timer::now(), rt.manager.link()).unwrap());
        rt.manager.start(rt.board, 0).unwrap();
        assert_ne!(vm.epoch(), epoch);

        park(&f, 0, &mut local); // No timer in the new incarnation.
        for _ in 0..4 {
            assert!(rt.dispatch(&mut local).unwrap());
        }
        assert_eq!(steps::<3>(&f), [1, 7, 9]);
    }
}
