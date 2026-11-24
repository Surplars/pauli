//! Validate native submissions once and publish operation-specific descriptor chains.
//! Ownership/Lock Ordering: reserve short slot lock -> unlocked staging -> locked publication.
//! Invariants: flush runs alone; header/write bytes reach DMA before avail.idx/notify.

use super::*;

impl<I: LocalIrq> VirtioBlk<I> {
    fn validate(&self, request: BlockSubmission, write: &[u8]) -> Result<()> {
        if request.operation == BlockOperation::Flush {
            if !self.supports_flush() {
                return Err(DeviceError::NotSupported);
            }
            if request.bytes != 0 || request.sector != 0 || !write.is_empty() {
                return Err(DeviceError::InvalidResource);
            }
            return Ok(());
        }
        if request.bytes == 0
            || request.bytes > TRANSFER_BYTES
            || request.bytes % SECTOR_BYTES != 0
            || request
                .sector
                .checked_add((request.bytes / SECTOR_BYTES) as u64)
                .is_none_or(|end| end > self.sectors)
        {
            return Err(DeviceError::InvalidResource);
        }
        match request.operation {
            BlockOperation::Read if !write.is_empty() => return Err(DeviceError::InvalidResource),
            BlockOperation::Write if !self.writable() => return Err(DeviceError::ReadOnly),
            BlockOperation::Write if write.len() != request.bytes => {
                return Err(DeviceError::InvalidResource);
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn submit_request(
        &self,
        request: BlockSubmission,
        write: &[u8],
    ) -> Result<BlockToken> {
        self.validate(request, write)?;
        let token = self.reserve(request)?;
        let staged = self.stage(token.slot(), request, write);
        let mut state = self.state.lock();
        if staged.is_err() || state.failed || state.recovering {
            self.check_token(&mut state, token)?.phase = Phase::Free;
            return Err(staged.err().unwrap_or(DeviceError::NotReady));
        }
        let slot = self.check_token(&mut state, token)?;
        self.prepare_chain(token.slot(), request);
        if let Err(error) = self.dma.for_device(
            token.slot() * CHAIN_SIZE * 16,
            CHAIN_SIZE * 16,
            Direction::ToDevice,
        ) {
            slot.phase = Phase::Free; // Still CPU-owned; avail.idx has not been published.
            return Err(error);
        }
        slot.phase = Phase::Pending;
        // SAFETY: one queue owner; available ring is CPU-owned, fixed indices fit.
        unsafe {
            let available = self.available();
            core::ptr::addr_of_mut!((*available).ring[state.available as usize % QUEUE_SIZE])
                .write_volatile(((token.slot() * CHAIN_SIZE) as u16).to_le());
            fence(Ordering::Release);
            core::ptr::addr_of_mut!((*available).idx)
                .write_volatile(state.available.wrapping_add(1).to_le());
        }
        state.available = state.available.wrapping_add(1);
        if let Err(error) = self.dma.for_device(
            AVAIL_OFFSET,
            core::mem::size_of::<VirtqAvail<QUEUE_SIZE>>(),
            Direction::ToDevice,
        ) {
            state.failed = true; // Already published: only reset ACK may reclaim.
            state.slots[token.slot()].phase = Phase::Detached;
            return Err(error);
        }
        self.transport.notify_queue(0);
        Ok(token)
    }

    fn reserve(&self, request: BlockSubmission) -> Result<BlockToken> {
        let mut state = self.state.lock();
        if state.failed {
            return Err(DeviceError::Fault);
        }
        if state.recovering {
            return Err(DeviceError::NotReady);
        }
        let active = |slot: &Slot| {
            matches!(
                slot.phase,
                Phase::Staging | Phase::Pending | Phase::Detached
            )
        };
        if state.slots.iter().any(|slot| {
            active(slot)
                && (request.operation == BlockOperation::Flush
                    || slot.request.operation == BlockOperation::Flush)
        }) {
            return Err(DeviceError::Busy);
        }
        let index = state
            .slots
            .iter()
            .position(|slot| slot.phase == Phase::Free)
            .ok_or(DeviceError::Busy)?;
        let slot = &mut state.slots[index];
        let generation = slot
            .generation
            .checked_add(1)
            .ok_or(DeviceError::Exhausted)?;
        *slot = Slot {
            generation,
            phase: Phase::Staging,
            request,
            result: Ok(0),
        };
        Ok(BlockToken::new(
            self.transport.base_addr(),
            generation,
            index as u8,
        ))
    }

    fn stage(&self, slot: usize, request: BlockSubmission, write: &[u8]) -> Result<()> {
        let base = Self::request_offset(slot);
        let kind = match request.operation {
            BlockOperation::Read => READ,
            BlockOperation::Write => WRITE,
            BlockOperation::Flush => FLUSH,
        };
        // SAFETY: Staging exclusively owns this fixed aligned slot; no device can
        // access it before publication. Only write bytes are copied, never guest DMA.
        unsafe {
            let header = self.dma.pointer(base);
            header.cast::<u32>().write_volatile(kind.to_le());
            header.add(4).cast::<u32>().write_volatile(0);
            header
                .add(8)
                .cast::<u64>()
                .write_volatile(request.sector.to_le());
            header.add(STATUS_OFFSET).write_volatile(STATUS_PENDING);
            if !write.is_empty() {
                core::ptr::copy_nonoverlapping(
                    write.as_ptr(),
                    header.add(HEADER_BYTES),
                    write.len(),
                );
            }
        }
        self.dma
            .for_device(base, HEADER_BYTES + request.bytes, Direction::Bidirectional)?;
        self.dma
            .for_device(base + STATUS_OFFSET, 1, Direction::Bidirectional)
    }

    fn prepare_chain(&self, slot: usize, request: BlockSubmission) {
        let kind = match request.operation {
            BlockOperation::Read => READ,
            BlockOperation::Write => WRITE,
            BlockOperation::Flush => FLUSH,
        };
        let head = slot * CHAIN_SIZE;
        // SAFETY: native queue lock excludes reset/other publishers; the slot
        // remains CPU-owned until the subsequent release store to avail.idx.
        unsafe {
            core::ptr::addr_of_mut!((*self.descriptor().add(head)).next)
                .write_volatile(((head + if kind == FLUSH { 2 } else { 1 }) as u16).to_le());
            core::ptr::addr_of_mut!((*self.descriptor().add(head + 1)).len)
                .write_volatile((request.bytes as u32).to_le());
            core::ptr::addr_of_mut!((*self.descriptor().add(head + 1)).flags).write_volatile(
                (desc_flags::NEXT | if kind == READ { desc_flags::WRITE } else { 0 }).to_le(),
            );
        }
    }
}
