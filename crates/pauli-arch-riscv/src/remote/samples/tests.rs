//! Exercise the same drain/observation ordering as the RISC-V owner without a clock or CSR.
use super::*;
use crate::remote::{FenceBatch, FenceQueue};

#[test]
fn record_only_after_target_sync_and_before_ticket_ack() {
    let queue = FenceQueue::new();
    let samples = Samples::new();
    let synced = Cell::new(false);
    let ticket = queue.request(true);
    queue.arm(3);
    assert_eq!(
        queue.drain_with(
            |batch| {
                assert_eq!(batch, FenceBatch::single(true));
                assert!(samples.get(true, 1).is_none());
                synced.set(true);
            },
            |prior, next, notify| {
                samples.record(prior, next, notify, || {
                    assert!(synced.get() && !queue.completed(ticket));
                    0 // TIME zero is valid; a separate requester tag marks presence.
                });
            }
        ),
        1 << 3
    );
    assert!(queue.completed(ticket));
    assert_eq!(samples.get(true, 1), Some((3, 0)));
    assert_eq!(samples.get(false, 1), None);
}

#[test]
fn mixed_coalesced_ambiguous_and_late_registrations_have_no_timestamp() {
    let queue = FenceQueue::new();
    let samples = Samples::new();
    let drain = || {
        queue.drain_with(
            |_| {},
            |old, new, notify| {
                samples.record(old, new, notify, || {
                    panic!("ambiguous batch read the clock")
                });
            },
        )
    };
    queue.request(false);
    queue.request(true);
    queue.arm(0);
    drain(); // Both kinds.
    queue.request(false);
    queue.request(false);
    queue.arm(1);
    drain(); // Two generations of the same kind, even with a single waiter.
    queue.request(true);
    queue.arm(0);
    queue.arm(1);
    drain(); // Ambiguous requester.
    queue.request(true);
    assert_eq!(drain(), 0); // No waiter.
    queue.arm(2);
    assert_eq!(drain(), 4); // A waiter-only drain is not the fence.
    for generation in 1..=3 {
        for instruction in [false, true] {
            assert_eq!(samples.get(instruction, generation), None);
        }
    }
}

#[test]
fn first_window_never_wraps_or_leaks_into_a_replacement_mailbox() {
    let mut samples = Samples::new();
    let end = LIMIT as u64;
    samples.record([end - 1, 0], [end, 0], 1 << 4, || 99);
    assert_eq!(samples.get(false, LIMIT), Some((4, 99)));
    for (before, after) in [(end, end + 1), (u64::MAX, 0), (0, 1)] {
        samples.record([before, 0], [after, 0], 1, || panic!("window reopened"));
    }
    assert_eq!(samples.get(false, 0), None);
    assert_eq!(samples.get(false, 1), None);
    assert_eq!(samples.get(false, LIMIT + 1), None);
    // The other kind has its own window. Cold restart constructs the entire
    // mailbox anew; identical target indices/generations cannot retain old ticks.
    samples.record([end, 0], [end, 1], 2, || 11);
    assert_eq!(samples.get(true, 1), Some((1, 11)));
    samples = Samples::new();
    assert_eq!(samples.get(false, LIMIT), None);
    assert_eq!(samples.get(true, 1), None);
    samples.record([0, 0], [0, 1], 4, || 23);
    assert_eq!(samples.get(true, 1), Some((2, 23)));
}

#[test]
fn requester_classifies_its_exact_completed_ticket_once() {
    let samples = Samples::new();
    let queue = FenceQueue::new();
    let direct = queue.request(false);
    let deferred = queue.request(true);
    let other = queue.request(true);
    samples.note_ready(direct, 2, true); // Too early: no target observation yet.
    samples.record([0, 0], [1, 0], 1 << 2, || 10);
    samples.note_ready(direct, 3, true);
    assert_eq!(samples.deferred(false, 1), None);
    samples.note_ready(direct, 2, false);
    samples.note_ready(direct, 2, true); // A duplicate cannot rewrite the first path.
    samples.record([1, 0], [1, 1], 1 << 2, || 20);
    samples.note_ready(other, 2, true); // Correct kind, wrong generation.
    assert_eq!(samples.deferred(true, 1), None);
    samples.note_ready(deferred, 2, true);
    assert_eq!(samples.deferred(false, 1), Some(false));
    assert_eq!(samples.deferred(true, 1), Some(true));
    assert_eq!(samples.deferred(true, 2), None);
    assert_eq!(samples.deferred(true, 0), None);
    assert_eq!(samples.deferred(true, LIMIT + 1), None);
}

#[test]
fn closed_window_accepts_its_last_ticket_but_not_a_reused_generation() {
    let mut samples = Samples::new();
    let ticket = |generation| FenceTicket {
        instruction: false,
        generation,
    };
    let limit = LIMIT as u64;
    samples.record([0, 0], [1, 0], 2, || 10);
    samples.record([limit - 1, 0], [limit, 0], 2, || 20);
    samples.note_ready(ticket(limit), 1, true);
    assert_eq!(samples.deferred(false, LIMIT), Some(true));
    samples.record([u64::MAX, 0], [0, 0], 2, || unreachable!());
    samples.note_ready(ticket(1), 1, false); // Old, unclassified slot cannot be filled.
    assert_eq!(samples.deferred(false, 1), None);
    samples = Samples::new();
    assert_eq!(samples.deferred(false, LIMIT), None);
    samples.record([0, 0], [1, 0], 2, || 30);
    samples.note_ready(ticket(1), 1, false);
    assert_eq!(samples.deferred(false, 1), Some(false));
}
