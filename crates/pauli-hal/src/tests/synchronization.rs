//! IRQ nesting, publication and completion-hint concurrency checks.
//! Ownership/Lock Ordering: test-owned locks; never install real IRQ hooks.
//! Invariants: unlock precedes restore, and a failed try_lock restores its own token.

use super::{FakeIrq, IRQ_ENABLED};
use crate::{
    device::CompletionEvent,
    sync::{IrqSpinLock, Once},
};
use std::{sync::Arc, thread, vec::Vec};

#[test]
fn irq_nesting_and_try_lock() {
    let first = IrqSpinLock::<_, FakeIrq>::new(0);
    let second = IrqSpinLock::<_, FakeIrq>::new(0);
    let outer = first.lock();
    assert!(!IRQ_ENABLED.with(|flag| flag.get()));
    let inner = second.lock();
    assert!(first.try_lock().is_none());
    drop(inner);
    assert!(!IRQ_ENABLED.with(|flag| flag.get()));
    drop(outer);
    assert!(IRQ_ENABLED.with(|flag| flag.get()));
}

#[test]
fn concurrent_lock_and_once() {
    let counter = Arc::new(IrqSpinLock::<_, FakeIrq>::new(0usize));
    let once = Arc::new(Once::new());
    let threads: Vec<_> = (0..4)
        .map(|id| {
            let counter = counter.clone();
            let once = once.clone();
            thread::spawn(move || {
                let installed = once.set(id).is_ok();
                for _ in 0..10_000 {
                    *counter.lock() += 1;
                }
                assert!(IRQ_ENABLED.with(|flag| flag.get()));
                installed
            })
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .filter_map(|thread| thread.join().unwrap().then_some(()))
            .count(),
        1
    );
    assert_eq!(*counter.lock(), 40_000);
    assert!(once.get().is_some());
    assert!(once.set(9).is_err());
}

#[test]
fn completion_hint_coalesces_without_losing_recheck() {
    let event = Arc::new(CompletionEvent::new());
    assert!(!event.take());
    event.notify();
    event.notify();
    assert!(event.take());
    assert!(!event.take());
    let writer = event.clone();
    let producer = thread::spawn(move || {
        for _ in 0..1000 {
            writer.notify();
        }
    });
    producer.join().unwrap();
    assert!(event.take());
    assert!(!event.take());
}
