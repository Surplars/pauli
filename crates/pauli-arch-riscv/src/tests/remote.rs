//! Production algorithm acceptance, using private host fixtures.
//! Ownership/Lock Ordering: each test owns its state; no actual CSR access.
//! Invariants: retain the original protocol assertions and rejection cases.
use crate::remote::fence;
use fence::{FenceBatch, FenceQueue};
use std::{
    sync::{
        Barrier,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[test]
fn transitions() {
    let q = FenceQueue::new();
    assert!(!q.pending());
    assert_eq!(q.drain(|_| panic!("empty queue fenced")), 0);
    let first = q.request(false);
    assert!(q.pending());
    q.arm(0);
    let mut later = None;
    assert_eq!(
        q.drain(|batch| {
            assert_eq!(batch, FenceBatch::single(false));
            assert!(!q.completed(first), "publication is not completion");
            // A request made during the actual fence is NOT covered by its earlier
            // snapshot; its registration must survive for the next drain.
            let next = q.request(true);
            q.arm(1);
            later = Some(next);
        }),
        1
    );
    let later = later.unwrap();
    assert!(q.completed(first));
    assert!(!q.completed(later));
    assert_eq!(
        q.drain(|batch| {
            assert_eq!(batch, FenceBatch::single(true));
            assert!(!q.completed(later));
        }),
        2
    );
    assert!(q.completed(later));
    assert!(!q.pending());

    // ACK can win before the waiter registers. A second drain must still
    // return the notification even though no additional fence is needed.
    let early = q.request(false);
    q.drain(|_| {});
    assert!(q.completed(early));
    q.arm(2);
    assert!(
        q.pending(),
        "late registration must not use the empty fast path"
    );
    assert_eq!(q.drain(|_| panic!("already covered")), 4);

    let a = q.request(false);
    q.arm(0);
    let b = q.request(true);
    q.arm(1);
    q.disarm(0);
    assert_eq!(
        q.drain(|batch| assert!(batch.translation && batch.instruction)),
        2
    );
    assert!(q.completed(a) && q.completed(b));
    assert_eq!(q.drain(|_| panic!("empty queue fenced")), 0);
}

#[test]
fn high_completion_bits_survive_cancel_and_late_registration() {
    let q = FenceQueue::new();
    let ticket = q.request(true);
    for id in [8, 9, 15] {
        q.arm(id);
    }
    q.disarm(9);
    assert_eq!(
        q.drain(|_| assert!(!q.completed(ticket))),
        (1 << 8) | (1 << 15)
    );
    assert!(q.completed(ticket));
    q.arm(15);
    assert_eq!(q.drain(|_| panic!("late waiter repeated a fence")), 1 << 15);
    assert!(!q.pending());
}

#[test]
fn mixed_batches() {
    // All eight-request kind interleavings. A shared callback must receive the
    // union, not just "instruction=true", before EITHER generation is ACKed.
    for kinds in 0u16..256 {
        let q = FenceQueue::new();
        let tickets = core::array::from_fn::<_, 8, _>(|index| {
            let ticket = q.request(kinds & (1 << index) != 0);
            q.arm((index % 4) as u8);
            ticket
        });
        let mut next = None;
        assert_eq!(
            q.drain(|batch| {
                assert_eq!(batch.translation, kinds != 255);
                assert_eq!(batch.instruction, kinds != 0);
                assert!(tickets.iter().all(|ticket| !q.completed(*ticket)));
                // Both newer kinds arrive during the old physical batch. Neither
                // may inherit its ACK, even when the old batch has the same flags.
                let t = q.request(false);
                let i = q.request(true);
                q.arm(0);
                next = Some((t, i));
            }),
            15
        );
        assert!(tickets.iter().all(|ticket| q.completed(*ticket)));
        let (t, i) = next.unwrap();
        assert!(!q.completed(t) && !q.completed(i));
        assert_eq!(
            q.drain(|batch| {
                assert!(batch.translation && batch.instruction);
                assert!(!q.completed(t) && !q.completed(i));
            }),
            1
        );
        assert!(q.completed(t) && q.completed(i));
        assert_eq!(q.drain(|_| panic!("empty batch")), 0);
    }
}

#[test]
fn concurrent() {
    const PRODUCERS: usize = 4;
    const ROUNDS: usize = 4096;
    let q = FenceQueue::new();
    let wakes = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let effects = [AtomicUsize::new(0), AtomicUsize::new(0)];
    let start = Barrier::new(PRODUCERS + 1);
    let deadline = Instant::now() + Duration::from_secs(20);
    thread::scope(|scope| {
        for requester in 0..PRODUCERS {
            let (q, wakes, done, start, effects) = (&q, &wakes, &done, &start, &effects);
            scope.spawn(move || {
                let bit = 1 << requester;
                start.wait();
                for round in 0..ROUNDS {
                    wakes.fetch_and(!bit, Ordering::AcqRel);
                    let instruction = (round + requester) % 2 == 0;
                    let before = effects[usize::from(instruction)].load(Ordering::Acquire);
                    let ticket = q.request(instruction);
                    q.arm(requester as u8);
                    // Recheck after arming: an ACK may precede registration.
                    while !q.completed(ticket) {
                        // Do NOT poll completion while asleep. The model must
                        // deliver a wake, just as an idle HS owner needs SSIP.
                        while wakes.fetch_and(!bit, Ordering::AcqRel) & bit == 0 {
                            assert!(Instant::now() < deadline, "completion wake lost");
                            thread::yield_now();
                        }
                    }
                    assert!(
                        effects[usize::from(instruction)].load(Ordering::Acquire) > before,
                        "ACK without corresponding fence kind"
                    );
                    q.disarm(requester as u8);
                }
                done.fetch_add(1, Ordering::Release);
            });
        }
        start.wait();
        while done.load(Ordering::Acquire) != PRODUCERS {
            let notify = if q.pending() {
                q.drain(|batch| {
                    if batch.translation {
                        effects[0].fetch_add(1, Ordering::Release);
                    }
                    if batch.instruction {
                        effects[1].fetch_add(1, Ordering::Release);
                    }
                    thread::yield_now();
                })
            } else {
                0
            };
            wakes.fetch_or(notify, Ordering::Release);
            assert!(Instant::now() < deadline, "producer did not finish");
            thread::yield_now();
        }
    });
    std::println!(
        "HV remote queue PASS: 256 mixed batches, ACK/arm/cancel races, {} concurrent requests with kind-specific effects and completion-only wakes",
        PRODUCERS * ROUNDS
    );
}
