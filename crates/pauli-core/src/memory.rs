//! Exclusive foreign guest RAM and private boot arena allocation.
//! Ownership/Lock Ordering: one device owner performs guest CPU copies; no DMA here.
//! Invariants: every access is checked and volatile; destructive writes require join.
use crate::{HvError, Result};
use core::ops::Range;
use pauli_config::{FIRMWARE_PAGE, FirmwareLayout, MAX_GUEST_DTB};
use pauli_devices::memory::GuestMemory;
use pauli_hal::mem::PhysRange;
mod copy;
mod extents;
pub mod pool;
mod restore;
mod slice;
pub(crate) use restore::RamImage;
pub(crate) use slice::ArenaSlice;

pub struct Arena {
    remaining: &'static mut [u8],
}
impl Arena {
    pub fn new(storage: &'static mut [u8]) -> Self {
        Self { remaining: storage }
    }
    pub fn take(&mut self, size: usize, align: usize) -> Result<&'static mut [u8]> {
        if !align.is_power_of_two() {
            return Err(HvError::InvalidMemory);
        }
        let base = self.remaining.as_ptr() as usize;
        let padding = base.wrapping_neg() & (align - 1);
        let required = padding.checked_add(size).ok_or(HvError::InvalidMemory)?;
        if required > self.remaining.len() {
            return Err(HvError::Exhausted);
        }
        let storage = core::mem::take(&mut self.remaining);
        let (_, aligned) = storage.split_at_mut(padding);
        let (allocation, rest) = aligned.split_at_mut(size);
        self.remaining = rest;
        Ok(allocation)
    }
}
pub struct GuestRam {
    host: PhysRange,
    guest: usize,
    writable: Range<usize>,
}
// SAFETY: this exclusive handle owns foreign bytes, not Rust references; native
// owners synchronize accesses through the VM device owner, never physical DMA.
unsafe impl Send for GuestRam {}
impl GuestRam {
    /// # Safety
    /// Retained identity-mapped foreign RAM is exclusively assigned to this VM,
    /// excluded from host allocation/images/devices and all other VM mappings.
    /// No aliased Rust reference exists, even while guest hardware mutates bytes.
    pub unsafe fn new(host: PhysRange, guest: usize) -> Result<Self> {
        guest
            .checked_add(host.size())
            .ok_or(HvError::InvalidMemory)?;
        Ok(Self {
            host,
            guest,
            writable: 0..host.size(),
        })
    }
    /// Apply the firmware stage-2 data-page permissions before publishing a device handle.
    pub(crate) fn for_firmware(mut self, layout: FirmwareLayout) -> Result<Self> {
        let page = FIRMWARE_PAGE as usize;
        let rom = (usize::from(layout.rx_pages) + usize::from(layout.ro_pages)) * page;
        let dtb = self
            .len()
            .checked_sub(MAX_GUEST_DTB)
            .ok_or(HvError::InvalidMemory)?;
        if layout.rx_pages == 0 || !self.len().is_multiple_of(page) || rom >= dtb {
            return Err(HvError::InvalidMemory);
        }
        self.writable = rom..dtb;
        Ok(self)
    }
    pub fn host_range(&self) -> PhysRange {
        self.host
    }
    pub fn guest_base(&self) -> usize {
        self.guest
    }
    pub fn len(&self) -> usize {
        self.host.size()
    }
    fn address(&self, gpa: u64, len: usize, write: bool) -> Option<usize> {
        let offset = usize::try_from(gpa).ok()?.checked_sub(self.guest)?;
        let end = offset.checked_add(len)?;
        let allowed = if write {
            self.writable.clone()
        } else {
            0..self.len()
        };
        if offset < allowed.start || end > allowed.end {
            return None;
        }
        self.host.base().checked_add(offset)
    }
}
impl GuestMemory for GuestRam {
    fn check(&self, gpa: u64, len: usize, write: bool) -> bool {
        self.address(gpa, len, write).is_some()
    }
    fn read(&self, gpa: u64, output: &mut [u8]) -> bool {
        let Some(host) = self.address(gpa, output.len(), false) else {
            return false;
        };
        // SAFETY: the checked foreign RAM is retained; output is an exclusive Rust slice.
        unsafe {
            copy::read(host, output);
        }
        true
    }
    fn write(&mut self, gpa: u64, input: &[u8]) -> bool {
        let Some(host) = self.address(gpa, input.len(), true) else {
            return false;
        };
        // SAFETY: checked retained foreign bytes, serialized host writer, no DMA alias.
        unsafe {
            copy::write(host, input);
        }
        true
    }
}
