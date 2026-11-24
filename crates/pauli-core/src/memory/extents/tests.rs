//! Compare the real in-RAM free list to a page ownership oracle.
//! Ownership/Lock Ordering: one test owns all backing and returns only its own receipts.
//! Invariants: firmware pages stay untouched; split, reuse and coalesce preserve exact capacity.
use super::*;
use pauli_hal::mem::PhysicalMemory;
use std::{vec, vec::Vec};

#[test]
fn full_boot_table_still_hands_off_more_than_48_disjoint_allocations() {
    const P: usize = PAGE_SIZE;
    const N: usize = 192;
    let mut backing = vec![0xa5u8; (N + 1) * P];
    let base = (backing.as_mut_ptr() as usize).next_multiple_of(P);
    let mut boot = PhysicalMemory::<48>::new(base, N * P).unwrap();
    // An unaligned one-byte firmware reservation must exclude the entire containing page.
    for i in (0..48).rev() {
        boot.reserve_range(base + 3 * i * P + 73, 1).unwrap();
    }
    assert!(boot.allocate(P, P).is_err());
    // SAFETY: this private allocation outlives the list; no Rust access to transferred pages.
    let mut list = unsafe { Extents::from_gaps(boot.free_ranges()) };
    let capacity = list.capacity(P, P);
    assert_eq!(capacity, N - 48);
    let mut owned = Vec::new();
    while let Some(r) = list.take(P, P, false) {
        let i = (r.base() - base) / P;
        assert!(i >= 3 * 48 || i % 3 != 0);
        assert!(!owned.iter().any(|prior| r.overlaps(*prior)));
        // SAFETY: this whole page has left the list; overwrite even its former header.
        unsafe { (r.base() as *mut u8).write_bytes(0x7b, P) };
        owned.push(r);
    }
    assert_eq!(owned.len(), capacity);
    for r in owned.into_iter().rev() {
        // SAFETY: returned exactly once; never mapped and no test writer remains.
        unsafe { list.give(r) };
    }
    assert_eq!(list.capacity(P, P), capacity);
    drop(list);
    let offset = base - backing.as_ptr() as usize;
    for i in 0..48 {
        assert!(
            backing[offset + 3 * i * P..offset + (3 * i + 1) * P]
                .iter()
                .all(|&b| b == 0xa5)
        );
    }
}

fn count_free(pages: &[bool], base: usize, size: usize, align: usize) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i + size <= pages.len() {
        if (base + i).is_multiple_of(align) && pages[i..i + size].iter().all(|p| *p) {
            count += 1;
            i += size;
        } else {
            i += 1;
        }
    }
    count
}

#[test]
fn fragmented_low_and_high_first_fit_match_an_independent_page_oracle() {
    const N: usize = 96;
    let p = PAGE_SIZE;
    let mut backing = vec![0u8; (N + 1) * p];
    let base = (backing.as_mut_ptr() as usize).next_multiple_of(p);
    let range = PhysRange::new(base, N * p).unwrap();
    // SAFETY: this private contiguous allocation remains retained until the list drops.
    let mut list = unsafe { Extents::from_gaps(core::iter::once(range)) };
    let mut free = [true; N];
    let mut owned: Vec<PhysRange> = vec![];
    let mut seed = 0x3c9a_e712u32;
    for _ in 0..900 {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let size = 1 + (seed >> 16) as usize % 7;
        let align = 1usize << ((seed >> 25) & 3);
        let possible = count_free(&free, base / p, size, align);
        assert_eq!(list.capacity(size * p, align * p), possible);
        if seed % 3 == 0 && !owned.is_empty() {
            let r = owned.swap_remove(seed as usize % owned.len());
            let i = (r.base() - base) / p;
            free[i..i + r.size() / p].fill(true);
            // SAFETY: swap_remove transferred the only ownership receipt, without readers.
            unsafe { list.give(r) };
        } else {
            let r = list.take(size * p, align * p, seed & 1 != 0);
            assert_eq!(r.is_some(), possible != 0);
            if let Some(r) = r {
                let i = (r.base() - base) / p;
                assert!(r.base().is_multiple_of(align * p));
                assert!(free[i..i + size].iter().all(|page| *page));
                free[i..i + size].fill(false);
                owned.push(r);
            }
        }
    }
    for r in owned {
        // SAFETY: this test retained each disjoint allocation and never mapped it.
        unsafe { list.give(r) };
    }
    assert_eq!(list.take(N * p, p, false), Some(range));
    assert!(list.take(p, p, true).is_none());
}
