//! Stable pin cells and owner placement in demand-allocated groups of VM IDs.
//! Ownership/Lock Ordering: one manager publishes; dispatch reads bounded atomic snapshots.
//! Invariants: groups never move; live bits are hints, only a current cell pin authorizes access.
use super::cell::{Cell, Reservation};
use crate::{
    HvError, Result,
    arch::MAX_CPUS,
    memory::pool::DomainPool,
    placement::{GROUP, Placement, Published},
};
use core::{
    mem::size_of_val,
    sync::atomic::{AtomicUsize, Ordering},
};
use pauli_hal::sync::Once;
#[cfg(test)]
mod tests;

use pauli_hal::addr::PAGE_SIZE;
type Page<T> = Once<Batch<T>>;
// Keep the directory page lazy even on a small host; wide hosts fit fewer
// groups. There is one hot placement word set per *participating* owner.
const MAX_GROUPS_PER_PAGE: usize = 16;
const _: () = assert!(size_of::<Group<()>>() + MAX_CPUS * size_of::<Published>() <= PAGE_SIZE);

struct Batch<T: 'static> {
    groups: &'static [Group<T>],
    lanes: &'static [Published],
}
impl<T> Batch<T> {
    fn view(&self, i: usize, owners: usize) -> Option<(&Group<T>, &[Published])> {
        Some((
            self.groups.get(i)?,
            &self.lanes[i * owners..(i + 1) * owners],
        ))
    }
}

pub(super) struct Group<T> {
    cells: [Cell<T>; GROUP],
    live: AtomicUsize,
}
impl<T> Group<T> {
    pub(super) const fn new() -> Self {
        Self {
            cells: [const { Cell::new() }; GROUP],
            live: AtomicUsize::new(0),
        }
    }
}
pub(super) struct Directory<T: 'static> {
    first: Batch<T>,
    second: Page<T>,
    extra: &'static [Page<T>],
    pool: Option<&'static DomainPool>,
    /// Trim dispatch/list work after the last live ID in a high group retires.
    end: AtomicUsize,
    assigned: [AtomicUsize; MAX_CPUS],
    limit: usize,
    page_shift: u32,
}
impl<T> Directory<T> {
    pub(super) fn new(
        first: &'static Group<T>,
        lanes: &'static [Published],
        pool: Option<&'static DomainPool>,
        limit: usize,
    ) -> Result<Self> {
        // Power-of-two grouping keeps runtime division out of dispatch lookups.
        let fits = PAGE_SIZE / (size_of::<Group<T>>() + size_of_val(lanes));
        let page_shift = fits.min(MAX_GROUPS_PER_PAGE).ilog2();
        let per_page = 1 << page_shift;
        let pages = limit.div_ceil(GROUP).saturating_sub(1).div_ceil(per_page);
        let count = pages.saturating_sub(1);
        let extra: &'static [Page<T>] = match pool {
            Some(pool) => pool.metadata(count, Once::new)?,
            None if pages == 0 => &[],
            None => return Err(HvError::InvalidConfig),
        };
        Ok(Self {
            first: Batch {
                groups: core::slice::from_ref(first),
                lanes,
            },
            second: Once::new(),
            extra,
            pool,
            end: AtomicUsize::new(1),
            assigned: [const { AtomicUsize::new(0) }; MAX_CPUS],
            limit,
            page_shift,
        })
    }
    fn group(&self, n: usize) -> Option<(&Group<T>, &[Published])> {
        let owners = self.first.lanes.len();
        if n == 0 {
            self.first.view(0, owners)
        } else {
            let (page, offset) = self.page_position(n);
            self.page(page)?.get()?.view(offset, owners)
        }
    }
    fn page_position(&self, n: usize) -> (usize, usize) {
        let i = n - 1; // Group zero is embedded in the boot store.
        (i >> self.page_shift, i & ((1 << self.page_shift) - 1))
    }
    fn page(&self, page: usize) -> Option<&Page<T>> {
        if page == 0 {
            Some(&self.second)
        } else {
            self.extra.get(page - 1)
        }
    }
    fn ensure(&self, n: usize) -> Result<&Group<T>> {
        if let Some((group, _)) = self.group(n) {
            return Ok(group);
        }
        let (page, offset) = self.page_position(n);
        let entry = self.page(page).ok_or(HvError::Exhausted)?;
        let pool = self.pool.ok_or(HvError::Exhausted)?;
        let remaining = self.limit.div_ceil(GROUP) - 1 - (page << self.page_shift);
        let count = remaining.min(1 << self.page_shift);
        let (groups, lanes) = pool.metadata_pair(
            count,
            Group::new,
            count * self.first.lanes.len(),
            Published::empty,
        )?;
        let groups = &*groups;
        entry
            .set(Batch { groups, lanes })
            .map_err(|_| HvError::InvalidState)?;
        Ok(&groups[offset])
    }
    pub(super) fn reserve(&self, boot: usize) -> Result<(usize, Reservation<'_, T>)> {
        for n in 0..self.limit.div_ceil(GROUP) {
            let base = n * GROUP;
            let group = self.ensure(n)?;
            let range = boot.saturating_sub(base)..(self.limit - base).min(GROUP);
            let used = group.live.load(Ordering::Relaxed);
            for i in range {
                if used & (1 << i) == 0
                    && let Some(pin) = group.cells[i].reserve()
                {
                    return Ok((base + i, pin));
                }
            }
        }
        Err(HvError::Exhausted)
    }
    pub(super) fn cell(&self, slot: usize) -> Option<&Cell<T>> {
        if slot >= self.limit {
            return None;
        }
        Some(&self.group(slot / GROUP)?.0.cells[slot % GROUP])
    }
    pub(super) fn limit(&self) -> usize {
        self.limit
    }
    pub(super) fn groups(&self) -> usize {
        self.end.load(Ordering::Acquire)
    }
    pub(super) fn has_assignments(&self, owner: usize) -> bool {
        self.assigned[owner].load(Ordering::Acquire) != 0
    }
    pub(super) fn placement(&self, n: usize, owner: usize) -> Placement {
        self.group(n)
            .and_then(|(_, lanes)| lanes.get(owner))
            .map_or(Placement::EMPTY, Published::snapshot)
    }
    pub(super) fn live(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.groups()).flat_map(|n| {
            let mut bits = self.live_word(n);
            core::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let i = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                Some(n * GROUP + i)
            })
        })
    }
    fn live_word(&self, n: usize) -> usize {
        self.group(n)
            .map_or(0, |(g, _)| g.live.load(Ordering::Acquire))
    }
    /// Only the sole writer calls this, after successful reservation or while removing
    /// a pinned-out VM. A reader can see the previous hints but cannot enter a stale VM.
    pub(super) fn set(&self, slot: usize, masks: [usize; MAX_CPUS], live: bool) {
        let n = slot / GROUP;
        let i = slot % GROUP;
        let (group, lanes) = self
            .group(n)
            .expect("publisher must own an initialized group");
        for (owner, (placed, mask)) in lanes.iter().zip(masks).enumerate() {
            if mask != 0 {
                placed.set_vm(i, if live { mask } else { 0 });
                // One publisher; include stopped VMs until the last pin retires.
                let assigned = &self.assigned[owner];
                let old = assigned.load(Ordering::Relaxed);
                assigned.store(if live { old + 1 } else { old - 1 }, Ordering::Release);
            }
        }
        let bit = 1usize << i;
        let old = group.live.load(Ordering::Relaxed);
        let mask = if live { old | bit } else { old & !bit };
        group.live.store(mask, Ordering::Release);
        let end = self.end.load(Ordering::Relaxed);
        if live && n >= end {
            self.end.store(n + 1, Ordering::Release);
        } else if mask == 0 && n + 1 == end && n != 0 {
            let last = (1..n).rfind(|&g| self.live_word(g) != 0).unwrap_or(0);
            self.end.store(last + 1, Ordering::Release);
        }
    }
}
