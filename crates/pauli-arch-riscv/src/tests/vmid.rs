//! VMID reuse, siblings and retirement assertion scenarios.
//! Ownership/Lock Ordering: independent modeled harts per test.
//! Invariants: invalidations include stale implicit translations.
use super::vmid_model::{Hart, key};
use crate::{
    remote::fence::{FenceBatch, FenceQueue},
    vmid::policy::{INVALIDATE, Key, Plan, TAGGED, VS_INVALIDATE},
};
#[test]
fn transitions() {
    let a = key(1);
    let b = key(2);
    let mut h = Hart::new(4);
    assert_eq!(h.cache.find(a), None);
    let first = h.enter(a, 0, true);
    assert_eq!(first.flags(), TAGGED | INVALIDATE);
    assert_eq!(
        h.enter(a, 0, false),
        Plan {
            tag: first.tag,
            flush: false,
            vs_flush: false,
        }
    );
    // A secondary/HSM-start context must clear stale per-hart state even
    // when another sibling retained this VM's same root/epoch binding.
    assert!(h.enter(a, 1, true).flush);
    assert!(!h.enter(a, 1, false).flush);
    assert!(h.enter(b, 0, true).flush);
    assert!(!h.enter(a, 1, false).flush);
    assert_eq!(h.stats.entry_flushes, 3);
    assert_eq!(h.stats.warm_entries, 3);

    let mut one = Hart::new(1);
    let old = one.enter(a, 0, false);
    let reused = one.enter(b, 0, false);
    assert_eq!(old.tag, reused.tag);
    assert!(reused.flush);
    assert_eq!(one.cache.find(a), None);
    one.synchronize(a, false); // Already evicted: no new binding or stale ACK.
    assert_eq!(one.cache.find(a), None);
    assert!(!one.enter(b, 0, false).flush);

    // Same root/HPA after cold reload is NOT the same lifetime.
    let restart = Key {
        epoch: 91,
        root: a.root,
    };
    one.enter(a, 0, false);
    assert!(one.enter(restart, 9, false).flush);
    assert!(!one.enter(restart, 9, false).flush);
    // A changed root also cannot hit an old epoch's retained binding.
    assert!(
        one.enter(
            Key {
                epoch: restart.epoch,
                root: b.root
            },
            3,
            false
        )
        .flush
    );
}
#[test]
fn unmanaged_contexts() {
    let a = key(1);
    let b = key(2);
    let mut h = Hart::new(4);
    for raw in [
        Key {
            epoch: 0,
            root: a.root,
        },
        Key {
            epoch: 0,
            root: b.root,
        },
    ] {
        assert_eq!(
            h.enter(raw, 0, false),
            Plan {
                tag: 0,
                flush: true,
                vs_flush: true,
            }
        );
        assert_eq!(
            h.enter(raw, 1, false),
            Plan {
                tag: 0,
                flush: true,
                vs_flush: true,
            }
        );
    }
    let mut no_ids = Hart::new(0);
    for version in 0..16 {
        assert_eq!(
            no_ids.enter(a, version, false),
            Plan {
                tag: 0,
                flush: true,
                vs_flush: true,
            }
        );
    }
}
#[test]
fn remote() {
    let a = key(1);
    let b = key(2);
    let mut p = Hart::new(4);
    let mut q = Hart::new(4);
    p.enter(a, 1, true);
    p.enter(b, 1, true);
    q.enter(b, 1, true);
    q.enter(a, 1, true);
    // Virtual target identity is stable, numeric VMIDs are deliberately not.
    assert_ne!(p.cache.find(a), q.cache.find(a));
    for hart in [&mut p, &mut q] {
        let before_b = hart.cache.find(b).unwrap();
        let queue = FenceQueue::new();
        let ticket = queue.request(false);
        queue.arm(0);
        assert!(!queue.completed(ticket));
        assert_eq!(
            queue.drain(|batch| {
                assert!(
                    !queue.completed(ticket),
                    "ACK preceded physical invalidation"
                );
                assert_eq!(batch, FenceBatch::single(false));
                hart.synchronize_batch(a, batch);
            }),
            1
        );
        assert!(queue.completed(ticket));
        // Only A's target tag was invalidated; unrelated B stays usable.
        assert_eq!(hart.vstage[usize::from(before_b)], Some((b, 0, 1)));
        assert!(!hart.enter(a, 2, false).flush);
        assert!(!hart.enter(b, 1, false).flush);
        let i = queue.request(true);
        queue.arm(0);
        assert_eq!(
            queue.drain(|batch| {
                assert!(!queue.completed(i));
                hart.synchronize_batch(a, batch);
            }),
            1
        );
        assert!(queue.completed(i));
        assert_eq!(hart.stats.remote_icache, 1);
        assert_eq!(hart.stats.remote_tlb, 1);
        assert_eq!(hart.stats.remote_syncs, 2);
        queue.disarm(0);
    }
    // A local numeric tag can be reused while a different hart keeps the
    // same number for B. No new entry may rely on that other hart's flush.
    let mut p = Hart::new(1);
    let mut q = Hart::new(1);
    p.enter(a, 1, true);
    q.enter(b, 1, true);
    p.enter(
        Key {
            epoch: 17,
            root: a.root,
        },
        8,
        false,
    );
    assert!(!q.enter(b, 1, false).flush);
    assert!(q.enter(a, 9, false).flush);
}
#[test]
fn exhaustive() {
    let keys = [
        key(0),
        key(1),
        key(2),
        key(3),
        key(4),
        Key {
            epoch: 19,
            root: key(1).root,
        },
    ];
    // Every six-entry interleaving: tag exhaustion, unmanaged fixtures, cold
    // restart at an identical root, and repeated HSM-style fresh contexts.
    let sequences = keys.len().pow(6);
    for capacity in 0..=4 {
        for sequence in 0..sequences {
            let mut hart = Hart::new(capacity);
            let mut encoded = sequence;
            for step in 0..6 {
                let key = keys[encoded % keys.len()];
                encoded /= keys.len();
                let fresh = (sequence + step) % 7 == 0;
                let cpu = ((sequence / 7 + step) % 3) as u8;
                hart.enter_cpu(key, cpu, key.epoch as u64 * 4 + u64::from(cpu), fresh);
            }
        }
    }
    std::println!(
        "VMID policy PASS: {} exhaustive owner sequences, zero/one/multiple tags, vCPU ASID collisions, reuse, cold/HSM restart, target-local RFENCE before ACK and retirement before RAM reuse",
        sequences * 5
    );
}
#[test]
fn retirement() {
    let old = key(1);
    let peer = key(2);
    let fresh = Key {
        epoch: 123,
        root: old.root,
    };
    for capacity in 0..=4 {
        let mut p = Hart::new(capacity);
        let mut q = Hart::new(capacity);
        p.enter(old, 1, false);
        q.enter(old, 1, false);
        // Guest exit/join alone is NOT an implicit-walk retirement barrier.
        if capacity != 0 {
            assert!(p.has_walk(old) && q.has_walk(old));
        }
        let mut outstanding = 0b11;
        p.retire(old);
        outstanding &= !1;
        assert_ne!(outstanding, 0, "RAM reuse must wait for the other owner");
        if capacity != 0 {
            assert!(q.has_walk(old));
        }
        q.retire(old);
        outstanding &= !2;
        assert_eq!(outstanding, 0);
        assert!(!p.has_walk(old) && !q.has_walk(old));
        if capacity != 0 {
            assert_eq!(p.cache.find(old), None);
            assert_eq!(q.cache.find(old), None);
        }
        // Only now may old page-table/backing addresses be overwritten.
        assert!(p.enter(fresh, 99, false).flush);
        assert!(q.enter(fresh, 99, false).flush);
        p.synchronize(old, false); // A delayed old identity cannot select fresh.
        assert_eq!(p.stats.retirements, 1);
        if capacity != 0 {
            assert!(!p.enter(fresh, 99, false).flush);
        }
        q.enter(peer, 3, false);
        p.retire(fresh);
        q.retire(fresh);
        assert!(!p.has_walk(fresh) && !q.has_walk(fresh));
    }
    let mut hart = Hart::new(4);
    hart.enter(old, 1, false);
    hart.enter(peer, 7, false);
    hart.retire(old);
    assert!(
        !hart.enter(peer, 7, false).flush,
        "retirement must not discard a peer binding"
    );
    assert!(hart.enter(fresh, 99, false).flush);
    assert!(!hart.enter(peer, 7, false).flush);
}

#[test]
fn first_fetch_is_local_to_the_owner_and_image_lifetime() {
    use core::cell::Cell;

    let fences = Cell::new([0; 2]);
    let enter = |mask, hart: usize| {
        crate::vmid::entering_hart(mask, hart, || {
            let mut counts = fences.get();
            counts[hart] += 1;
            fences.set(counts);
        })
    };
    // A failed prepare discards its proposed mask. Neither an off-CPU peer
    // nor a warm resumption can satisfy another hart's first fetch.
    let _failed = enter(0, 1);
    let mut mask = enter(0, 1);
    mask = enter(mask, 0);
    mask = enter(mask, 1);
    assert_eq!((mask, fences.get()), (0b11, [1, 2]));

    // A local retirement invalidates only its bit; a cold restart starts empty.
    mask &= !1;
    mask = enter(mask, 1);
    mask = enter(mask, 0);
    assert_eq!((mask, fences.get()), (0b11, [2, 2]));
    assert_eq!(enter(0, 1), 0b10);
    assert_eq!(fences.get(), [2, 3]);
}
#[test]
fn sibling_asids() {
    let a = key(1);
    let b = key(2);
    for capacity in 1..=4 {
        let mut hart = Hart::new(capacity);
        let first = hart.enter_cpu(a, 0, 11, false);
        assert!(first.flush);
        // Same guest ASID on virtual hart 1 refers to a different VS root.
        // Preserve G-stage but never reuse virtual hart 0's VS-only mapping.
        let sibling = hart.enter_cpu(a, 1, 44, false);
        assert_eq!(sibling.tag, first.tag);
        assert_eq!(sibling.flags(), TAGGED | VS_INVALIDATE);
        assert_eq!(hart.enter_cpu(a, 1, 44, false).flags(), TAGGED);
        assert_eq!(
            hart.enter_cpu(a, 0, 11, false).flags(),
            TAGGED | VS_INVALIDATE
        );
        hart.enter_cpu(b, 1, 88, false);
        if capacity > 1 {
            // Switching VMs does not destroy A's recorded VS context.
            assert_eq!(hart.enter_cpu(a, 0, 11, false).flags(), TAGGED);
        }
        // A fence to the off-CPU sibling covers the VM tag too; it must not
        // accidentally flush HS tag zero or change the selected context.
        hart.synchronize(a, false);
        hart.enter_cpu(a, 0, 12, false);
        assert_eq!(hart.enter_cpu(a, 1, 45, true).flags(), TAGGED | INVALIDATE);
    }
}
#[test]
fn reuse_vacant_binding() {
    let mut hart = Hart::new(4);
    let keys = [key(11), key(12), key(13), key(14)];
    for key in keys {
        hart.enter(key, 0, false);
    }
    let freed = hart.cache.find(keys[1]).unwrap();
    hart.retire(keys[1]);
    // The round-robin victim is slot zero, but slot one is now unoccupied.
    assert_eq!(hart.enter(key(15), 0, false).tag, freed);
    for peer in [keys[0], keys[2], keys[3]] {
        assert_eq!(hart.enter(peer, 0, false).flags(), TAGGED);
    }
}
#[test]
fn mixed_remote() {
    let a = key(81);
    let b = key(82);
    for capacity in 0..=4 {
        let mut hart = Hart::new(capacity);
        hart.enter(b, 7, false);
        hart.enter(a, 1, false);
        let before = hart.stats;
        let q = FenceQueue::new();
        let t = q.request(false);
        let i = q.request(true);
        q.arm(0);
        q.arm(1);
        assert_eq!(
            q.drain(|batch| {
                assert!(batch.translation && batch.instruction);
                assert!(!q.completed(t) && !q.completed(i));
                hart.synchronize_batch(a, batch);
                assert_eq!(hart.stats.remote_tlb, before.remote_tlb + 1);
                assert_eq!(hart.stats.remote_icache, before.remote_icache + 1);
            }),
            3
        );
        assert!(q.completed(t) && q.completed(i));
        // A modified PTE needs a new VS translation on the same warm binding.
        // The previous FENCE.I-only callback would ACK both tickets but fail
        // this assertion because the cached version=1 remained in the TLB.
        hart.enter(a, 2, false);
        if capacity > 1 {
            hart.enter(b, 7, false);
        }
        // Retired/evicted TLB residency is already gone. A mixed batch still
        // MUST execute its instruction half, without touching a recycled tag.
        hart.retire(a);
        let fresh = Key {
            epoch: 99,
            root: a.root,
        };
        hart.enter(fresh, 11, false);
        let before = hart.stats;
        hart.synchronize_batch(
            a,
            FenceBatch {
                translation: true,
                instruction: true,
            },
        );
        assert_eq!(hart.stats.remote_icache, before.remote_icache + 1);
        assert_eq!(
            hart.stats.remote_tlb,
            before.remote_tlb + u64::from(capacity == 0)
        );
        hart.enter(fresh, 11, false);
    }
}
