//! Board-validated host MMIO windows, never guest-supplied physical addresses.
//! Ownership/Lock Ordering: drivers serialize device-specific register sequences.
//! Invariants: windows stay mapped for their lifetime; checked APIs reject bad widths/ranges.

use crate::device::{DeviceError, DeviceResource, Result};
use core::ptr::NonNull;

pub struct Mmio {
    base: NonNull<u8>,
    size: usize,
}
// SAFETY: mapping lifetime is established by new; drivers own register serialization.
unsafe impl Send for Mmio {}
// SAFETY: register accesses are volatile; compound sequences remain a driver's duty.
unsafe impl Sync for Mmio {}

impl Mmio {
    /// # Safety
    /// The resource must describe mapped host device memory, accessible at the
    /// given widths for this object's lifetime, not RAM owned by a guest.
    pub unsafe fn new(resource: DeviceResource, minimum: usize) -> Result<Self> {
        if resource.size < minimum || resource.base.checked_add(resource.size).is_none() {
            return Err(DeviceError::InvalidResource);
        }
        Ok(Self {
            base: NonNull::new(resource.base as *mut u8).ok_or(DeviceError::InvalidResource)?,
            size: resource.size,
        })
    }
    pub fn base(&self) -> usize {
        self.base.as_ptr() as usize
    }
    pub fn size(&self) -> usize {
        self.size
    }
    pub fn read8(&self, offset: usize) -> Result<u8> {
        self.check(offset, 1)?;
        // SAFETY: constructor supplies the mapping; check covers this byte.
        Ok(unsafe { self.base.as_ptr().add(offset).read_volatile() })
    }
    pub fn write8(&self, offset: usize, value: u8) -> Result<()> {
        self.check(offset, 1)?;
        // SAFETY: constructor supplies the mapping; check covers this byte.
        unsafe {
            self.base.as_ptr().add(offset).write_volatile(value);
        }
        Ok(())
    }
    pub fn read32(&self, offset: usize) -> Result<u32> {
        self.check(offset, 4)?;
        // SAFETY: checked word alignment and range inside the persistent MMIO window.
        Ok(u32::from_le(unsafe { self.read32_unchecked(offset) }))
    }
    pub fn write32(&self, offset: usize, value: u32) -> Result<()> {
        self.check(offset, 4)?;
        // SAFETY: checked word alignment and range inside the persistent MMIO window.
        unsafe {
            self.write32_unchecked(offset, value.to_le());
        }
        Ok(())
    }
    fn check(&self, offset: usize, width: usize) -> Result<()> {
        if self.base().wrapping_add(offset) % width != 0
            || offset.checked_add(width).is_none_or(|end| end > self.size)
        {
            return Err(DeviceError::InvalidResource);
        }
        Ok(())
    }
    /// # Safety
    /// Caller proves four mapped, naturally aligned bytes at this offset.
    pub unsafe fn read32_unchecked(&self, offset: usize) -> u32 {
        // SAFETY: caller's checked register layout covers the word.
        unsafe { self.base.as_ptr().add(offset).cast::<u32>().read_volatile() }
    }
    /// # Safety
    /// Caller proves four mapped, naturally aligned bytes at this offset.
    pub unsafe fn write32_unchecked(&self, offset: usize, value: u32) {
        // SAFETY: caller's checked register layout covers the word.
        unsafe {
            self.base
                .as_ptr()
                .add(offset)
                .cast::<u32>()
                .write_volatile(value);
        }
    }
}
