#![no_std]
//! Modern VirtIO-MMIO transport and split-ring wire structures.
//! Ownership/Lock Ordering: a native driver owns register sequencing, no locks here.
//! Invariants: queue addresses come only from owned DMA regions; no guest MMIO proxying.

use core::sync::atomic::{Ordering, fence};
use pauli_hal::{
    device::{DeviceError, DeviceResource, Result},
    mmio::Mmio,
};
pub mod regs;
mod ring;
pub use ring::{VirtqAvail, VirtqDesc, VirtqUsed, VirtqUsedElem};

pub const DEVICE_ID_BLOCK: u32 = 2;
pub const DEVICE_ID_NET: u32 = 1;
pub const RESET_POLLS: usize = 1024;
pub mod status {
    //! VirtIO device status bits; immutable constants, no state or locks.
    //! Invariants: FAILED/NEEDS_RESET are never interpreted as a usable queue.
    pub const ACKNOWLEDGE: u32 = 1;
    pub const DRIVER: u32 = 2;
    pub const DRIVER_OK: u32 = 4;
    pub const FEATURES_OK: u32 = 8;
    pub const NEEDS_RESET: u32 = 64;
    pub const FAILED: u32 = 128;
}
pub mod desc_flags {
    //! Split descriptor flags; immutable constants, no state or locks.
    //! Invariants: no indirect or packed descriptors are negotiated.
    pub const NEXT: u16 = 1;
    pub const WRITE: u16 = 2;
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeatureSet {
    pub lo: u32,
    pub hi: u32,
}
impl FeatureSet {
    pub const MODERN: u32 = 1;
}

pub struct MmioTransport {
    mmio: Mmio,
}
impl MmioTransport {
    /// # Safety
    /// Resource is mapped native device memory for this transport's lifetime,
    /// not a guest address. Its base must be naturally word-aligned.
    pub unsafe fn new(resource: DeviceResource) -> Result<Self> {
        if resource.base & 3 != 0 {
            return Err(DeviceError::InvalidResource);
        }
        // SAFETY: caller supplies the persistent mapping; bounds/alignment checked once.
        Ok(Self {
            mmio: unsafe { Mmio::new(resource, regs::MIN_BYTES)? },
        })
    }
    pub fn base_addr(&self) -> usize {
        self.mmio.base()
    }
    pub fn device_id(&self) -> u32 {
        self.read32(regs::DEVICE_ID)
    }
    pub fn validate_device(&self, expected: u32) -> Result<()> {
        if self.read32(regs::MAGIC) != regs::MAGIC_VALUE
            || self.read32(regs::VERSION) != regs::MODERN_VERSION
            || self.device_id() != expected
        {
            return Err(DeviceError::NotSupported);
        }
        Ok(())
    }
    pub fn read_config32(&self, offset: usize) -> Result<u32> {
        self.mmio.read32(
            regs::CONFIG
                .checked_add(offset)
                .ok_or(DeviceError::InvalidResource)?,
        )
    }
    pub fn read_config64(&self, offset: usize) -> Result<u64> {
        let high_offset = offset.checked_add(4).ok_or(DeviceError::InvalidResource)?;
        Ok(u64::from(self.read_config32(offset)?)
            | (u64::from(self.read_config32(high_offset)?) << 32))
    }
    pub fn reset_and_wait(&self) -> Result<()> {
        self.write32(regs::STATUS, 0);
        fence(Ordering::SeqCst);
        for _ in 0..RESET_POLLS {
            if self.status() == 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(DeviceError::Timeout)
    }
    pub fn status(&self) -> u32 {
        self.read32(regs::STATUS)
    }
    pub fn set_status_bits(&self, bits: u32) {
        self.write32(regs::STATUS, self.status() | bits);
    }
    pub fn fail(&self) {
        self.set_status_bits(status::FAILED);
    }
    pub fn negotiate_features(&self, requested: FeatureSet) -> Result<FeatureSet> {
        self.write32(regs::DEVICE_FEATURES_SEL, 0);
        let lo = self.read32(regs::DEVICE_FEATURES);
        self.write32(regs::DEVICE_FEATURES_SEL, 1);
        let hi = self.read32(regs::DEVICE_FEATURES);
        if hi & FeatureSet::MODERN == 0 {
            return Err(DeviceError::NotSupported);
        }
        let selected = FeatureSet {
            lo: lo & requested.lo,
            hi: (hi & requested.hi) | FeatureSet::MODERN,
        };
        self.write32(regs::DRIVER_FEATURES_SEL, 0);
        self.write32(regs::DRIVER_FEATURES, selected.lo);
        self.write32(regs::DRIVER_FEATURES_SEL, 1);
        self.write32(regs::DRIVER_FEATURES, selected.hi);
        self.set_status_bits(status::FEATURES_OK);
        if self.status() & status::FEATURES_OK == 0 {
            return Err(DeviceError::NotSupported);
        }
        Ok(selected)
    }
    pub fn setup_queue(
        &self,
        queue: u32,
        size: usize,
        desc: u64,
        available: u64,
        used: u64,
    ) -> Result<()> {
        if !size.is_power_of_two()
            || size > u16::MAX as usize
            || desc & 15 != 0
            || available & 1 != 0
            || used & 3 != 0
        {
            return Err(DeviceError::InvalidResource);
        }
        self.write32(regs::QUEUE_SEL, queue);
        if (self.read32(regs::QUEUE_NUM_MAX) as usize) < size || self.read32(regs::QUEUE_READY) != 0
        {
            return Err(DeviceError::NotSupported);
        }
        self.write32(regs::QUEUE_NUM, size as u32);
        self.write_pair(regs::QUEUE_DESC_LOW, desc);
        self.write_pair(regs::QUEUE_DRIVER_LOW, available);
        self.write_pair(regs::QUEUE_DEVICE_LOW, used);
        self.write32(regs::QUEUE_READY, 1);
        Ok(())
    }
    pub fn notify_queue(&self, queue: u32) {
        self.write32(regs::QUEUE_NOTIFY, queue);
    }
    pub fn ack_interrupts(&self) -> u32 {
        let pending = self.read32(regs::INTERRUPT_STATUS);
        if pending != 0 {
            self.write32(regs::INTERRUPT_ACK, pending);
        }
        pending
    }
    fn read32(&self, offset: usize) -> u32 {
        // SAFETY: private callers pass named aligned registers below MIN_BYTES.
        u32::from_le(unsafe { self.mmio.read32_unchecked(offset) })
    }
    fn write32(&self, offset: usize, value: u32) {
        // SAFETY: private callers pass named aligned registers below MIN_BYTES.
        unsafe {
            self.mmio.write32_unchecked(offset, value.to_le());
        }
    }
    fn write_pair(&self, offset: usize, value: u64) {
        self.write32(offset, value as u32);
        self.write32(offset + 4, (value >> 32) as u32);
    }
}
