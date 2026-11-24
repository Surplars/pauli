//! Fixed-capacity host RAM reservations and aligned page-frame allocation.
//! Ownership/Lock Ordering: boot/VM manager owns mutations; no internal locks.
//! Invariants: every allocation is reserved, nonoverflowing and disjoint from firmware/host/peers.

use crate::addr::{MmError, PAGE_SIZE};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysRange {
    base: usize,
    size: usize,
}
impl PhysRange {
    pub const fn base(self) -> usize {
        self.base
    }
    pub const fn size(self) -> usize {
        self.size
    }
    pub fn new(base: usize, size: usize) -> Result<Self, MmError> {
        if size == 0 || base.checked_add(size).is_none() {
            return Err(MmError::InvalidRegion);
        }
        Ok(Self { base, size })
    }
    pub fn end(self) -> usize {
        self.base + self.size
    }
    pub fn overlaps(self, other: Self) -> bool {
        self.base < other.end() && other.base < self.end()
    }
    pub fn contains(self, other: Self) -> bool {
        other.base >= self.base && other.end() <= self.end()
    }
}

pub struct PhysicalMemory<const N: usize> {
    ram: PhysRange,
    reserved: [Option<PhysRange>; N],
}
impl<const N: usize> PhysicalMemory<N> {
    pub fn new(base: usize, size: usize) -> Result<Self, MmError> {
        Ok(Self {
            ram: PhysRange::new(base, size)?,
            reserved: [None; N],
        })
    }
    pub fn ram(&self) -> PhysRange {
        self.ram
    }
    pub fn reservations(&self) -> impl Iterator<Item = PhysRange> + '_ {
        self.reserved.iter().flatten().copied()
    }
    /// Enumerate disjoint gaps in address order, including unaligned edges.
    /// Only the boot handoff walks this small, unsorted firmware reservation table.
    pub fn free_ranges(&self) -> impl Iterator<Item = PhysRange> + '_ {
        let mut cursor = self.ram.base();
        core::iter::from_fn(move || {
            while cursor < self.ram.end() {
                let prev = cursor;
                let next = self
                    .reservations()
                    .filter(|r| r.base() >= prev)
                    .min_by_key(|r| r.base());
                let end = next.map_or(self.ram.end(), PhysRange::base);
                cursor = next.map_or(end, PhysRange::end);
                if end > prev {
                    return Some(PhysRange {
                        base: prev,
                        size: end - prev,
                    });
                }
            }
            None
        })
    }
    /// Boot-time admission ceiling. Simulate the real first-fit policy using
    /// reservation metadata only, accounting for holes, alignment and free records.
    pub fn allocation_capacity(&self, size: usize, alignment: usize) -> usize {
        let mut model = Self {
            ram: self.ram,
            reserved: self.reserved,
        };
        let mut count = 0;
        while model.allocate(size, alignment).is_ok() {
            count += 1;
        }
        count
    }
    pub fn reserve_range(&mut self, base: usize, size: usize) -> Result<PhysRange, MmError> {
        let range = PhysRange::new(base, size)?;
        if !self.ram.contains(range) {
            return Err(MmError::InvalidRegion);
        }
        if self.reservations().any(|used| used.overlaps(range)) {
            return Err(MmError::AlreadyMapped);
        }
        *self
            .reserved
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(MmError::OutOfMemory)? = Some(range);
        Ok(range)
    }
    /// Frame allocations never implicitly reclaim VM memory. A retired owner
    /// may release its exact range only after all CPUs have invalidated its TLBs.
    pub fn allocate(&mut self, size: usize, alignment: usize) -> Result<PhysRange, MmError> {
        if size == 0
            || size % PAGE_SIZE != 0
            || alignment < PAGE_SIZE
            || !alignment.is_power_of_two()
        {
            return Err(MmError::InvalidAlignment);
        }
        let mut next = align_up(self.ram.base, alignment)?;
        loop {
            let range = PhysRange::new(next, size)?;
            if !self.ram.contains(range) {
                return Err(MmError::OutOfMemory);
            }
            let conflict = self
                .reservations()
                .filter(|used| used.overlaps(range))
                .map(PhysRange::end)
                .max();
            match conflict {
                Some(end) => next = align_up(end, alignment)?,
                None => return self.reserve_range(next, size),
            }
        }
    }
    /// # Safety
    /// No CPU/device may still access the range, and all guest translations
    /// referring to it must have been retired on every participating CPU.
    pub unsafe fn release_retired(&mut self, range: PhysRange) -> Result<(), MmError> {
        let slot = self
            .reserved
            .iter_mut()
            .find(|slot| **slot == Some(range))
            .ok_or(MmError::NotMapped)?;
        *slot = None;
        Ok(())
    }
}
fn align_up(value: usize, alignment: usize) -> Result<usize, MmError> {
    value
        .checked_add(alignment - 1)
        .map(|next| next & !(alignment - 1))
        .ok_or(MmError::InvalidRegion)
}
