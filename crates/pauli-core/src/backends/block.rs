//! Four-request nonblocking block bridge with durable ordering and reset drain.
//! Ownership/Lock Ordering: VM devices -> native queue; copies use private staging.
//! Invariants: flush is ordered by frontend; stop drains writes before RAM/epoch reuse.
//! Lifecycle: Empty -> Admitted -> NativePending -> Collected -> Empty.
use crate::{
    HvError, Result,
    memory::{Arena, GuestRam},
};
use core::task::Poll;
use pauli_devices::virtio_blk::{BlockBackend, MAX_INFLIGHT, MAX_TRANSFER, VirtioBlock};
use pauli_hal::device::{
    BlockOperation, BlockSubmission, BlockToken, CompletionEvent, DeviceError, HostBlockDevice,
};
mod native;
mod retirement;
mod service;
use native::Backend;
use service::Service;
pub const REG_ACK: usize = 0x64;
pub const REG_STATUS: usize = 0x70;
pub const REG_QUEUE_READY: usize = 0x44;
pub const BUFFER_BYTES: usize = MAX_INFLIGHT * MAX_TRANSFER;
pub struct Block {
    pub device: VirtioBlock<&'static mut [u8]>,
    backend: Backend,
    schedule: Service,
    draining: bool,
    writable: bool,
    retirement: retirement::Retirement,
}
impl Block {
    pub fn bind(
        event: &'static CompletionEvent,
        native: &'static dyn HostBlockDevice,
        sectors: u64,
        writable: bool,
        hz: u64,
        arena: &mut Arena,
    ) -> Result<Self> {
        if native.sectors() != sectors
            || (writable && !(native.writable() && native.supports_flush()))
        {
            return Err(HvError::InvalidDevice);
        }
        let batch = native.max_transfer_bytes().min(MAX_TRANSFER) & !511;
        if batch == 0 || hz == 0 {
            return Err(HvError::InvalidDevice);
        }
        let timeout = hz.checked_mul(5).ok_or(HvError::InvalidConfig)?;
        let storage = arena.take(BUFFER_BYTES, 8)?;
        event.take();
        Ok(Self {
            device: VirtioBlock::with_buffer(sectors, writable, storage),
            backend: Backend::new(native, event, batch, timeout),
            schedule: Service::new((hz / 10_000).max(1)),
            draining: false,
            writable,
            retirement: retirement::Retirement::Live,
        })
    }
    pub fn access(&mut self, offset: usize, write: Option<usize>) -> Option<usize> {
        if write == Some(0) && matches!(offset, REG_STATUS | REG_QUEUE_READY) {
            self.draining = true;
        }
        let value = match write {
            Some(value) => self.device.write_word(offset, value as u32).map(|()| 0),
            None => self.device.read_word(offset).map(|value| value as usize),
        };
        if value.is_some() && write.is_some() && offset != REG_ACK {
            self.schedule.kick();
        }
        value
    }
    pub fn service(&mut self, memory: &mut GuestRam, now: u64) {
        let event = self.backend.event.take();
        if !self.has_work() {
            self.schedule.idle();
            return;
        }
        if !self.schedule.due(now, event) {
            return;
        }
        self.backend.now = now;
        if self.draining {
            match self.backend.drain_all() {
                Ok(true) => self.draining = false,
                _ => {
                    self.schedule.rearm(now, false, self.backend.deadline());
                    return;
                }
            }
        }
        let submitted = self.backend.submitted;
        let completed = self.device.service(memory, &mut self.backend, MAX_INFLIGHT);
        if self.has_work() {
            self.schedule.rearm(
                now,
                completed != 0 || submitted != self.backend.submitted,
                self.backend.deadline(),
            );
        } else {
            self.schedule.idle();
        }
    }
    fn has_work(&self) -> bool {
        self.draining
            || self.device.needs_service()
            || self.backend.pending.iter().any(Option::is_some)
            || self.backend.admission.iter().any(Option::is_some)
    }
    pub fn reset(&mut self) -> Result<()> {
        if self.backend.pending.iter().any(Option::is_some)
            || self.backend.quarantined
            || matches!(
                self.retirement,
                retirement::Retirement::Draining { .. } | retirement::Retirement::Flushing { .. }
            )
        {
            return Err(HvError::Busy);
        }
        self.device
            .write_word(REG_STATUS, 0)
            .ok_or(HvError::InvalidDevice)?;
        self.backend.event.take();
        self.retirement = retirement::Retirement::Live;
        self.draining = false;
        Ok(())
    }
}
