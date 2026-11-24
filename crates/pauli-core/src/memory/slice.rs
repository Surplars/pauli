//! Exact-length typed storage in one VM's private host arena.
//! Ownership/Lock Ordering: one builder initializes it; live readers borrow the owner.
//! Invariants: only the initialized prefix is visible or dropped, including on rollback.
use super::Arena;
use crate::{HvError, Result};
use core::{alloc::Layout, mem::MaybeUninit, ops::Deref, ptr, slice};
use pauli_hal::addr::PAGE_SIZE;

pub(crate) struct ArenaSlice<T: 'static> {
    storage: &'static mut [MaybeUninit<T>],
    live: usize,
}
impl<T> ArenaSlice<T> {
    /// Reserve whole pages at board admission. A preceding arch-specific table
    /// may leave the arena cursor unaligned for T, so include its worst padding.
    pub(crate) fn budget(count: usize) -> Result<usize> {
        let layout = Layout::array::<T>(count).map_err(|_| HvError::InvalidMemory)?;
        layout
            .size()
            .checked_add(layout.align() - 1)
            .and_then(|n| n.checked_next_multiple_of(PAGE_SIZE))
            .ok_or(HvError::InvalidMemory)
    }
    pub(crate) fn build(
        arena: &mut Arena,
        count: usize,
        mut init: impl FnMut(usize) -> Result<T>,
    ) -> Result<Self> {
        let layout = Layout::array::<T>(count).map_err(|_| HvError::InvalidMemory)?;
        let bytes = arena.take(layout.size(), layout.align())?;
        // SAFETY: take transferred an exclusive, suitably aligned span of exactly
        // count T-sized cells. MaybeUninit does not interpret old arena bytes as T.
        let storage = unsafe { slice::from_raw_parts_mut(bytes.as_mut_ptr().cast(), count) };
        let mut result = Self { storage, live: 0 };
        for i in 0..count {
            result.storage[i].write(init(i)?);
            result.live += 1;
        }
        Ok(result)
    }
}
impl<T> Deref for ArenaSlice<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        // SAFETY: build initialized this prefix; references cannot outlive the VM owner.
        unsafe { slice::from_raw_parts(self.storage.as_ptr().cast(), self.live) }
    }
}
impl<T> Drop for ArenaSlice<T> {
    fn drop(&mut self) {
        let used = ptr::slice_from_raw_parts_mut(self.storage.as_mut_ptr().cast::<T>(), self.live);
        // SAFETY: exclusive owner, exactly one drop per initialized element; the
        // physical receipt is released separately, after these destructors finish.
        unsafe { ptr::drop_in_place(used) };
    }
}
