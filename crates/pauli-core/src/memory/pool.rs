//! Runtime guest RAM and page-table reservations from the board's remaining RAM.
//! Ownership/Lock Ordering: lock only to reserve/free an extent, never while zeroing it.
//! Invariants: one receipt covers guest RAM plus disjoint host-only tables and VM state.
use super::{Arena, GuestRam, extents::Extents};
use crate::{
    HvError, Result,
    board::{BootImages, VmResources},
};
use pauli_config::VmConfig;
use pauli_hal::{
    addr::PAGE_SIZE,
    mem::{PhysRange, PhysicalMemory},
    sync::SpinLock,
};

use core::{alloc::Layout, mem::MaybeUninit};

/// Only firmware, boot guests and native DMA need records in the bootstrap table.
pub type FramePool = PhysicalMemory<48>;
// A page-aligned firmware window can also consume padding before the table root.
// The existing budget fits a 16 KiB root and four lower tables even with that gap.
pub const TABLE_ARENA_BYTES: usize = 64 * 1024;

pub struct DomainPool(SpinLock<Extents>);
pub(crate) struct Receipt {
    pool: &'static DomainPool,
    extent: PhysRange,
}
/// The VM object is allocated only for an actual child, never in its GPA range.
struct ExtentLayout {
    size: usize,
    align: usize,
    arena: usize,
    object: usize,
}
impl ExtentLayout {
    fn new<T>(bytes: usize, align: usize, metadata: usize) -> Result<Self> {
        if bytes == 0 || !bytes.is_multiple_of(align) {
            return Err(HvError::InvalidMemory);
        }
        let arena = TABLE_ARENA_BYTES
            .checked_add(metadata)
            .ok_or(HvError::InvalidMemory)?;
        let prefix = bytes.checked_add(arena).ok_or(HvError::InvalidMemory)?;
        let prefix = Layout::from_size_align(prefix, align).map_err(|_| HvError::InvalidMemory)?;
        let (layout, object) = prefix
            .extend(Layout::new::<T>())
            .map_err(|_| HvError::InvalidMemory)?;
        let size = layout
            .size()
            .checked_next_multiple_of(PAGE_SIZE)
            .ok_or(HvError::InvalidMemory)?;
        Ok(Self {
            size,
            align: layout.align(),
            arena,
            object,
        })
    }
}
fn ram_alignment(config: &VmConfig) -> usize {
    if config.firmware.is_some() {
        PAGE_SIZE
    } else {
        pauli_config::ALIGN as usize
    }
}
impl DomainPool {
    /// # Safety
    /// Every unreserved byte must be exclusively owned, writable identity-mapped
    /// RAM for the pool's lifetime. Reserve firmware, boot domains, templates,
    /// native DMA and all host allocations before handing off the allocator.
    pub unsafe fn from_boot(pool: FramePool) -> Self {
        // SAFETY: the final boot allocator excludes all retained owners. Only full
        // free pages transfer, and consuming the allocator prevents a second handoff.
        Self(SpinLock::new(unsafe {
            Extents::from_gaps(pool.free_ranges())
        }))
    }
    pub(crate) fn capacity<T>(
        &self,
        template: &VmConfig,
        minimum_metadata: usize,
    ) -> Result<usize> {
        // Firmware geometry is fixed by its linker/DTB contract. Linux may
        // request a smaller shape than the boot template.
        let bytes = if template.firmware.is_some() {
            template.memory_size
        } else {
            pauli_config::MIN_RAM_BYTES
        };
        let bytes = usize::try_from(bytes).map_err(|_| HvError::InvalidMemory)?;
        let layout = ExtentLayout::new::<T>(bytes, ram_alignment(template), minimum_metadata)?;
        Ok(self.0.lock().capacity(layout.size, layout.align))
    }
    /// Permanent host-only metadata (registry and grants). Published cells stay
    /// at stable addresses; reclaimable guest extents come from separate receipts.
    pub(crate) fn metadata<T: 'static>(
        &'static self,
        count: usize,
        init: impl Fn() -> T,
    ) -> Result<&'static mut [T]> {
        self.metadata_pair(count, init, 0, || ())
            .map(|(cells, _)| cells)
    }
    /// Registry IDs and their owner lanes must succeed as one reservation: a
    /// failed second allocation must not strand a permanent metadata page.
    pub(crate) fn metadata_pair<T: 'static, U: 'static>(
        &'static self,
        n: usize,
        make: impl Fn() -> T,
        m: usize,
        extra: impl Fn() -> U,
    ) -> Result<(&'static mut [T], &'static mut [U])> {
        if n == 0 && m == 0 {
            return Ok((&mut [], &mut []));
        }
        let a = Layout::array::<T>(n).map_err(|_| HvError::InvalidMemory)?;
        let b = Layout::array::<U>(m).map_err(|_| HvError::InvalidMemory)?;
        let (layout, offset) = a.extend(b).map_err(|_| HvError::InvalidMemory)?;
        let bytes = layout
            .size()
            .max(1)
            .checked_next_multiple_of(PAGE_SIZE)
            .ok_or(HvError::InvalidMemory)?;
        let span = self
            .0
            .lock()
            .take(bytes, layout.align().max(PAGE_SIZE), true)
            .ok_or(HvError::Exhausted)?;
        // SAFETY: Layout::extend describes disjoint, aligned subarrays of one
        // permanent exclusive host extent. Neither subarray is ever guest RAM.
        unsafe {
            Ok((
                init_metadata(span.base(), n, make),
                init_metadata(span.base() + offset, m, extra),
            ))
        }
    }
    pub(crate) fn allocate<T>(
        &'static self,
        config: &VmConfig,
        images: BootImages,
        metadata: usize,
    ) -> Result<(VmResources, &'static mut MaybeUninit<T>, Receipt)> {
        let guest = usize::try_from(config.guest_base).map_err(|_| HvError::InvalidMemory)?;
        let bytes = usize::try_from(config.memory_size).map_err(|_| HvError::InvalidMemory)?;
        let layout = ExtentLayout::new::<T>(bytes, ram_alignment(config), metadata)?;
        let extent = self
            .0
            .lock()
            .take(layout.size, layout.align, false)
            .ok_or(HvError::Exhausted)?;
        let receipt = Receipt { pool: self, extent };
        // Both subranges are inside the freshly reserved extent. The arena is
        // never mapped at a GPA, even when the same physical bank is recycled.
        let ram = PhysRange::new(extent.base(), bytes)
            .map_err(|_| HvError::InvalidMemory)
            .and_then(|range| {
                // SAFETY: exclusive subrange of this new, disjoint reservation.
                unsafe { GuestRam::new(range, guest) }
            });
        let ram = match ram {
            Ok(ram) => ram,
            Err(error) => {
                // SAFETY: no consumer, reference or mapping has been created.
                unsafe { receipt.release() };
                return Err(error);
            }
        };
        let ptr = ram.host_range().end() as *mut u8;
        // SAFETY: the private arena follows RAM within this allocation; no old
        // page-table reference survives release. Initialize before making a slice.
        let storage = unsafe {
            ptr.write_bytes(0, layout.arena);
            core::slice::from_raw_parts_mut(ptr, layout.arena)
        };
        // SAFETY: Layout::extend put this aligned, uninitialized object after the
        // private arena. Its extent remains reserved until the last VM pin is gone.
        let object = unsafe { &mut *((extent.base() + layout.object) as *mut MaybeUninit<T>) };
        Ok((
            VmResources {
                ram,
                arena: Arena::new(storage),
                images,
            },
            object,
            receipt,
        ))
    }
}

#[cfg(test)]
mod tests;

/// Caller owns the aligned, permanent backing for `count` elements exclusively.
unsafe fn init_metadata<T: 'static>(
    base: usize,
    count: usize,
    init: impl Fn() -> T,
) -> &'static mut [T] {
    // SAFETY: supplied layout covers these cells; old bytes need not represent T.
    let cells = unsafe { core::slice::from_raw_parts_mut(base as *mut MaybeUninit<T>, count) };
    for cell in cells.iter_mut() {
        cell.write(init());
    }
    // SAFETY: every cell was just initialized and no other reference survives.
    unsafe { cells.assume_init_mut() }
}
impl Receipt {
    /// # Safety
    /// All contexts, mappings, arena references and device producers must be gone.
    /// Every visited CPU must have retired translations, or the build never ran.
    /// Dropping a receipt alone intentionally does not grant permission to reuse.
    pub(crate) unsafe fn release(self) {
        // SAFETY: caller proved retirement; this unforgeable receipt names the
        // exact reservation in the same pool, never caller-selected host memory.
        unsafe { self.pool.0.lock().give(self.extent) };
    }
}
