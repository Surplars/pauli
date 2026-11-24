#![no_std]
//! Four-slot native VirtIO block engine with private, pinned bounce storage.
//! Ownership/Lock Ordering: VM device -> short native queue lock; copies run unlocked.
//! Invariants: no guest DMA; cancellation never recycles a device-owned slot.
//! Lifecycle: Free -> Staging -> Pending -> Complete -> Copying -> Free;
//! Pending -> Detached -> Free only after used completion or native reset ACK.

use core::sync::atomic::{Ordering, fence};
use pauli_hal::{
    device::{
        BlockOperation, BlockSubmission, BlockToken, DeviceError, DeviceResource, HostBlockDevice,
        Result,
    },
    dma::{Direction, DmaRegion},
    irq::IrqHandler,
    sync::{IrqSpinLock, LocalIrq},
};
use pauli_virtio_common::{
    DEVICE_ID_BLOCK, FeatureSet, MmioTransport, VirtqAvail, VirtqDesc, VirtqUsed, desc_flags,
    status,
};
mod collection;
mod completion;
mod queue;
mod state;
mod submit;
use state::{Phase, Slot, State};

pub const REQUEST_SLOTS: usize = 4;
pub const TRANSFER_BYTES: usize = 64 * 1024;
pub const SECTOR_BYTES: usize = 512;
const QUEUE_SIZE: usize = 16;
const CHAIN_SIZE: usize = 3;
const AVAIL_OFFSET: usize = QUEUE_SIZE * 16;
const USED_OFFSET: usize = 4096;
const REQUEST_OFFSET: usize = 8192;
const HEADER_BYTES: usize = 16;
const STATUS_OFFSET: usize = HEADER_BYTES + TRANSFER_BYTES;
const REQUEST_STRIDE: usize = (STATUS_OFFSET + 1 + 4095) & !4095;
pub const DMA_BYTES: usize = REQUEST_OFFSET + REQUEST_SLOTS * REQUEST_STRIDE;
const FEATURE_RO: u32 = 1 << 5;
const FEATURE_FLUSH: u32 = 1 << 9;
const READ: u32 = 0;
const WRITE: u32 = 1;
const FLUSH: u32 = 4;
const STATUS_OK: u8 = 0;
const STATUS_PENDING: u8 = 0xff;
const REQUESTED: FeatureSet = FeatureSet {
    lo: FEATURE_RO | FEATURE_FLUSH,
    hi: 0,
};

// Compile-time native DMA layout invariants, not guest-input assertions.
const _: () = {
    assert!(REQUEST_SLOTS * CHAIN_SIZE <= QUEUE_SIZE);
    assert!(AVAIL_OFFSET + core::mem::size_of::<VirtqAvail<QUEUE_SIZE>>() <= USED_OFFSET);
    assert!(USED_OFFSET + core::mem::size_of::<VirtqUsed<QUEUE_SIZE>>() <= REQUEST_OFFSET);
};

pub struct VirtioBlk<I: LocalIrq> {
    transport: MmioTransport,
    dma: DmaRegion,
    sectors: u64,
    features: u32,
    state: IrqSpinLock<State, I>,
}
impl<I: LocalIrq> VirtioBlk<I> {
    /// # Safety
    /// Resource is a persistent mapped native VirtIO-MMIO device. DMA is an
    /// exclusive coherent/cache-managed host region, never guest RAM, and remains
    /// pinned even after this driver is stopped. Register only static driver instances.
    pub unsafe fn new(resource: DeviceResource, dma: DmaRegion) -> Result<Self> {
        dma.check_layout(DMA_BYTES, 4096)?;
        // SAFETY: caller supplies native mapped device registers, not guest MMIO.
        let transport = unsafe { MmioTransport::new(resource)? };
        transport.validate_device(DEVICE_ID_BLOCK)?;
        transport.reset_and_wait()?;
        transport.set_status_bits(status::ACKNOWLEDGE | status::DRIVER);
        let features = transport.negotiate_features(REQUESTED)?.lo;
        let sectors = transport.read_config64(0)?;
        if sectors == 0 {
            return Err(DeviceError::InvalidResource);
        }
        let driver = Self {
            transport,
            dma,
            sectors,
            features,
            state: IrqSpinLock::new(State::new()),
        };
        driver.configure_queue()?;
        driver.transport.set_status_bits(status::DRIVER_OK);
        Ok(driver)
    }
    fn request_offset(slot: usize) -> usize {
        REQUEST_OFFSET + slot * REQUEST_STRIDE
    }
    fn descriptor(&self) -> *mut VirtqDesc {
        // SAFETY: new validated aligned DMA_BYTES; caller holds queue ownership.
        unsafe { self.dma.pointer(0).cast() }
    }
    fn available(&self) -> *mut VirtqAvail<QUEUE_SIZE> {
        // SAFETY: compile-time layout fits this aligned queue window.
        unsafe { self.dma.pointer(AVAIL_OFFSET).cast() }
    }
    fn used(&self) -> *mut VirtqUsed<QUEUE_SIZE> {
        // SAFETY: compile-time layout fits this aligned queue window.
        unsafe { self.dma.pointer(USED_OFFSET).cast() }
    }
    fn check_token<'a>(&self, state: &'a mut State, token: BlockToken) -> Result<&'a mut Slot> {
        if token.owner() != self.transport.base_addr() {
            return Err(DeviceError::Stale);
        }
        let slot = state
            .slots
            .get_mut(token.slot())
            .ok_or(DeviceError::Stale)?;
        if slot.generation != token.generation() || slot.phase == Phase::Free {
            return Err(DeviceError::Stale);
        }
        Ok(slot)
    }
}
impl<I: LocalIrq> HostBlockDevice for VirtioBlk<I> {
    fn sectors(&self) -> u64 {
        self.sectors
    }
    fn writable(&self) -> bool {
        self.features & FEATURE_RO == 0
    }
    fn supports_flush(&self) -> bool {
        self.features & FEATURE_FLUSH != 0
    }
    fn max_transfer_bytes(&self) -> usize {
        TRANSFER_BYTES
    }
    fn submit(&self, request: BlockSubmission, write: &[u8]) -> Result<BlockToken> {
        self.submit_request(request, write)
    }
    fn try_poll(&self, token: BlockToken) -> Result<Option<usize>> {
        self.poll_request(token)
    }
    fn collect(&self, token: BlockToken, out: &mut [u8]) -> Result<usize> {
        self.collect_request(token, out)
    }
    fn cancel(&self, token: BlockToken) -> Result<()> {
        self.cancel_request(token)
    }
    fn service(&self, now: u64) -> Result<usize> {
        let count = match self.service_used() {
            Ok(count) => count,
            Err(DeviceError::NotReady) => return Ok(0),
            Err(error) => {
                self.recover(error)?;
                return Err(error);
            }
        };
        let expired = self.state.lock().slots.iter().any(|slot| {
            matches!(slot.phase, Phase::Pending | Phase::Detached) && now >= slot.request.deadline
        });
        if expired {
            self.recover(DeviceError::Timeout)?;
        }
        Ok(count)
    }
}
impl<I: LocalIrq> IrqHandler for VirtioBlk<I> {
    fn handle_irq(&self) -> bool {
        let pending = self.transport.ack_interrupts();
        if pending == 0 {
            return false;
        }
        // IRQ only publishes bounded completions. Recovery/poll waits belong to
        // the dispatcher, never to an IRQ handler holding unrelated locks.
        let _ = self.service_used();
        true
    }
}

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests;
