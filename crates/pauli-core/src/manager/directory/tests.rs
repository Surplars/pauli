//! Scale the production identity directory independently of Linux guest RAM size.
//! Ownership/Lock Ordering: one publisher; a reader holds a high pin across a retirement attempt.
//! Invariants: unpublished/retired cells cannot be used, and sparse reuse never transfers authority.
use super::*;
use crate::memory::pool::FramePool;
use pauli_hal::addr::PAGE_SIZE;
use std::{boxed::Box, sync::Barrier, thread, vec};

fn directory(limit: usize) -> Directory<u32> {
    with_owners(limit, 2)
}
fn with_owners(limit: usize, owners: usize) -> Directory<u32> {
    // Only registry metadata is under test. Do not reserve gigabytes of unused guest RAM.
    let bytes = Box::leak(vec![0u8; 10 * PAGE_SIZE].into_boxed_slice());
    let frames = FramePool::new(bytes.as_mut_ptr() as usize, bytes.len()).unwrap();
    // SAFETY: backing is permanent, exclusively owned, with no firmware, DMA or guest aliases.
    let pool = Box::leak(Box::new(unsafe { DomainPool::from_boot(frames) }));
    Directory::new(
        Box::leak(Box::new(Group::new())),
        Box::leak((0..owners).map(|_| Published::empty()).collect()),
        Some(pool),
        limit,
    )
    .unwrap()
}

#[test]
fn demand_pages_fit_the_actual_hart_count_and_retire_only_the_last_assignment() {
    let small = with_owners(32, 1);
    let wide = with_owners(32, MAX_CPUS);
    assert!(small.page_shift > wide.page_shift);
    let last = MAX_CPUS - 1;
    for id in 0..20 {
        assert_eq!(publish(&wide, id + 1, last), id as usize);
    }
    let page = wide.second.get().unwrap();
    assert_eq!(page.lanes.len(), page.groups.len() * MAX_CPUS);
    assert!(size_of_val(page.lanes) + size_of_val(page.groups) <= PAGE_SIZE);
    for id in (1..20).rev() {
        assert!(wide.has_assignments(last));
        assert!(!wide.has_assignments(last - 1));
        remove(&wide, id, id as u32 + 1).unwrap();
    }
    assert!(wide.has_assignments(last));
    remove(&wide, 0, 1).unwrap();
    assert!(!wide.has_assignments(last));
}
fn publish(d: &Directory<u32>, epoch: u32, owner: usize) -> usize {
    let (id, claim) = d.reserve(0).unwrap();
    let mut masks = [0; MAX_CPUS];
    masks[owner] = 1 << 15;
    claim.publish(Box::leak(Box::new(core::mem::MaybeUninit::uninit())), epoch);
    d.set(id, masks, true);
    id
}
fn remove(d: &Directory<u32>, id: usize, epoch: u32) -> Result<()> {
    d.cell(id).unwrap().remove(|old| {
        if *old != epoch {
            return Err(HvError::Busy);
        }
        let masks =
            core::array::from_fn(|owner| d.placement(id / GROUP, owner).local_cpus(id % GROUP));
        d.set(id, masks, false);
        Ok(())
    })
}
#[test]
fn ninety_seven_identities_grow_lazily_and_shrink_the_live_scan() {
    const N: usize = 97;
    let d = directory(N);
    assert!(d.extra[0].get().is_none() && d.second.get().is_none());
    // Dropping an unpublished reservation cannot create a hole or stale live bit.
    drop(d.reserve(0).unwrap().1);
    for id in 0..N {
        assert_eq!(publish(&d, id as u32 + 1, id % 2), id);
        assert_eq!(d.second.get().is_some(), id >= GROUP);
        assert_eq!(
            d.extra[0].get().is_some(),
            id >= GROUP * (1 + (1 << d.page_shift))
        );
    }
    assert!(matches!(d.reserve(0), Err(HvError::Exhausted)));
    assert!(d.cell(N).is_none());
    assert!(d.live().eq(0..N));
    for id in 0..N {
        assert_eq!(*d.cell(id).unwrap().read().unwrap(), id as u32 + 1);
        assert_eq!(
            d.placement(id / GROUP, id % 2).local_cpus(id % GROUP),
            1 << 15
        );
        assert_eq!(
            d.placement(id / GROUP, 1 - id % 2).local_cpus(id % GROUP),
            0
        );
    }
    for id in (1..N).rev() {
        remove(&d, id, id as u32 + 1).unwrap();
    }
    assert_eq!(d.groups(), 1);
    assert!(d.live().eq([0]));
    let page = d.extra[0].get().unwrap().groups.as_ptr();
    for id in 1..N {
        assert_eq!(publish(&d, 1000 + id as u32, 1), id);
    }
    assert_eq!(d.extra[0].get().unwrap().groups.as_ptr(), page);
    assert_eq!(remove(&d, N - 1, N as u32), Err(HvError::Busy));
}

#[test]
fn a_high_identity_cannot_be_recycled_under_a_remote_reader() {
    const ID: usize = 70;
    let d = directory(ID + 1);
    for id in 0..=ID {
        assert_eq!(publish(&d, id as u32 + 1, 0), id);
    }
    let barrier = Barrier::new(2);
    thread::scope(|scope| {
        scope.spawn(|| {
            let pin = d.cell(ID).unwrap().read().unwrap();
            barrier.wait();
            barrier.wait();
            assert_eq!(*pin, ID as u32 + 1);
        });
        barrier.wait();
        assert_eq!(remove(&d, ID, ID as u32 + 1), Err(HvError::Busy));
        barrier.wait();
    });
    remove(&d, ID, ID as u32 + 1).unwrap();
    assert_eq!(publish(&d, 999, 1), ID);
    assert_eq!(remove(&d, ID, ID as u32 + 1), Err(HvError::Busy));
    assert_eq!(d.placement(ID / GROUP, 0).local_cpus(ID % GROUP), 0);
    assert_eq!(d.placement(ID / GROUP, 1).local_cpus(ID % GROUP), 1 << 15);
}
