//! Exclusive pinned native DMA storage and board-specific cache synchronization.
//! Ownership/Lock Ordering: device state lock owns CPU access; DMA handoff is explicit.
//! Invariants: no guest/caller buffer is mapped; reset ACK precedes reuse after cancellation.

use crate::device::{DeviceError, Result};
use core::{
    ptr::NonNull,
    sync::atomic::{Ordering, fence},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    ToDevice,
    FromDevice,
    Bidirectional,
}

/// Cache maintenance appropriate for the board's native DMA domain.
/// # Safety
/// Implementations must synchronize the entire supplied CPU range, including
/// required cache-line rounding/barriers, before the caller changes ownership.
pub unsafe trait DmaSync: Send + Sync {
    fn for_device(&self, cpu: usize, bytes: usize, direction: Direction);
    fn for_cpu(&self, cpu: usize, bytes: usize, direction: Direction);
}
pub struct Coherent;
// SAFETY: use only for coherent/uncached mappings, as required by DmaRegion::new.
unsafe impl DmaSync for Coherent {
    fn for_device(&self, _: usize, _: usize, _: Direction) {
        fence(Ordering::Release);
    }
    fn for_cpu(&self, _: usize, _: usize, _: Direction) {
        fence(Ordering::Acquire);
    }
}
pub static COHERENT: Coherent = Coherent;

pub struct DmaRegion {
    cpu: NonNull<u8>,
    device: u64,
    bytes: usize,
    sync: &'static dyn DmaSync,
}
// SAFETY: the exclusive static lease moves with this value; no CPU reference escapes.
unsafe impl Send for DmaRegion {}
// SAFETY: driver locks serialize CPU access; synchronization is part of every DMA handoff.
unsafe impl Sync for DmaRegion {}

impl DmaRegion {
    /// # Safety
    /// The entire buffer must be private pinned host storage, physically
    /// contiguous at device, with correct cache operations. Never supply guest
    /// RAM or stack/caller data. No other CPU/driver may access this lease.
    pub unsafe fn new(
        buffer: &'static mut [u8],
        device: u64,
        sync: &'static dyn DmaSync,
    ) -> Result<Self> {
        if buffer.is_empty() || device.checked_add(buffer.len() as u64).is_none() {
            return Err(DeviceError::InvalidResource);
        }
        Ok(Self {
            cpu: NonNull::from(&mut buffer[0]),
            device,
            bytes: buffer.len(),
            sync,
        })
    }
    pub fn len(&self) -> usize {
        self.bytes
    }
    pub fn is_empty(&self) -> bool {
        self.bytes == 0
    }
    pub fn cpu_address(&self) -> usize {
        self.cpu.as_ptr() as usize
    }
    pub fn device_address(&self, offset: usize) -> Result<u64> {
        if offset >= self.bytes {
            return Err(DeviceError::InvalidResource);
        }
        Ok(self.device + offset as u64)
    }
    pub fn check_layout(&self, bytes: usize, alignment: usize) -> Result<()> {
        if bytes > self.bytes
            || !alignment.is_power_of_two()
            || self.cpu_address() % alignment != 0
            || self.device % alignment as u64 != 0
        {
            return Err(DeviceError::InvalidResource);
        }
        Ok(())
    }
    pub fn for_device(&self, offset: usize, bytes: usize, direction: Direction) -> Result<()> {
        self.check(offset, bytes)?;
        self.sync
            .for_device(self.cpu_address() + offset, bytes, direction);
        Ok(())
    }
    pub fn for_cpu(&self, offset: usize, bytes: usize, direction: Direction) -> Result<()> {
        self.check(offset, bytes)?;
        self.sync
            .for_cpu(self.cpu_address() + offset, bytes, direction);
        Ok(())
    }
    fn check(&self, offset: usize, bytes: usize) -> Result<()> {
        if offset.checked_add(bytes).is_none_or(|end| end > self.bytes) {
            return Err(DeviceError::InvalidResource);
        }
        Ok(())
    }
    /// # Safety
    /// Offset must lie in this region and the driver must own the relevant
    /// bytes (or perform a volatile read of device-owned completion metadata).
    pub unsafe fn pointer(&self, offset: usize) -> *mut u8 {
        // SAFETY: static exclusive lease and caller-proven offset/access ownership.
        unsafe { self.cpu.as_ptr().add(offset) }
    }
}
