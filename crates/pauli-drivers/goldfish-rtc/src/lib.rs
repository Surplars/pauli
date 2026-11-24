#![no_std]
//! Native read-only Goldfish UTC source, independent of RTOS driver registration.
//! Ownership/Lock Ordering: short local IRQ-safe lock serializes LOW/HIGH latch pairs.
//! Invariants: no alarm/host clock writes; hardware is a mapped board-owned resource.

use pauli_hal::{
    device::{DeviceResource, HostRtcDevice, Result},
    mmio::Mmio,
    sync::{IrqSpinLock, LocalIrq},
};
const TIME_LOW: usize = 0;
const TIME_HIGH: usize = 4;
pub struct GoldfishRtc<I: LocalIrq> {
    mmio: Mmio,
    owner: IrqSpinLock<(), I>,
}
impl<I: LocalIrq> GoldfishRtc<I> {
    /// # Safety
    /// Resource must be a persistent mapped native Goldfish RTC, not guest MMIO.
    pub unsafe fn new(resource: DeviceResource) -> Result<Self> {
        // SAFETY: caller supplies mapped device memory; both latch words fit the checked window.
        let mmio = unsafe { Mmio::new(resource, 8)? };
        if resource.base & 3 != 0 {
            return Err(pauli_hal::device::DeviceError::InvalidResource);
        }
        Ok(Self {
            mmio,
            owner: IrqSpinLock::new(()),
        })
    }
}
impl<I: LocalIrq> HostRtcDevice for GoldfishRtc<I> {
    fn unix_ns(&self) -> Result<u64> {
        let _owner = self.owner.lock();
        let low = self.mmio.read32(TIME_LOW)?;
        let high = self.mmio.read32(TIME_HIGH)?;
        Ok(u64::from(low) | (u64::from(high) << 32))
    }
}

#[cfg(test)]
mod tests;
