//! Token-scoped collection/cancellation with copies outside the IRQ-off queue lock.
//! Ownership/Lock Ordering: mark Copying under lock -> copy private data -> release slot.
//! Invariants: cancelled pending DMA stays quarantined; no scoped read-view/profiling path.

use super::*;
impl<I: LocalIrq> VirtioBlk<I> {
    pub(super) fn poll_request(&self, token: BlockToken) -> Result<Option<usize>> {
        let mut state = self.state.lock();
        if state.failed {
            return Err(DeviceError::Fault);
        }
        let slot = self.check_token(&mut state, token)?;
        match slot.phase {
            Phase::Complete => slot.result.map(Some),
            Phase::Pending | Phase::Staging => Ok(None),
            Phase::Copying => Err(DeviceError::Busy),
            Phase::Free | Phase::Detached => Err(DeviceError::Stale),
        }
    }
    pub(super) fn collect_request(&self, token: BlockToken, out: &mut [u8]) -> Result<usize> {
        let request = {
            let mut state = self.state.lock();
            let slot = self.check_token(&mut state, token)?;
            if slot.phase != Phase::Complete {
                return Err(DeviceError::NotReady);
            }
            if let Err(error) = slot.result {
                slot.phase = Phase::Free;
                return Err(error);
            }
            if slot.request.operation == BlockOperation::Read && out.len() < slot.request.bytes {
                return Err(DeviceError::InvalidResource);
            }
            slot.phase = Phase::Copying;
            slot.request
        };
        let result = self.copy_read(token.slot(), request, out);
        self.check_token(&mut self.state.lock(), token)?.phase = Phase::Free;
        result
    }
    fn copy_read(&self, slot: usize, request: BlockSubmission, out: &mut [u8]) -> Result<usize> {
        if request.operation == BlockOperation::Read {
            let offset = Self::request_offset(slot) + HEADER_BYTES;
            self.dma
                .for_cpu(offset, request.bytes, Direction::FromDevice)?;
            // SAFETY: Copying exclusively retains this completed native slot;
            // cache sync preceded CPU access and the checked output cannot alias DMA.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.dma.pointer(offset),
                    out.as_mut_ptr(),
                    request.bytes,
                );
            }
        }
        Ok(request.bytes)
    }
    pub(super) fn cancel_request(&self, token: BlockToken) -> Result<()> {
        let mut state = self.state.lock();
        let slot = self.check_token(&mut state, token)?;
        match slot.phase {
            Phase::Pending => {
                slot.phase = Phase::Detached;
                slot.request.event = None;
            }
            Phase::Complete => slot.phase = Phase::Free,
            Phase::Detached => {}
            Phase::Copying => return Err(DeviceError::Busy),
            Phase::Free | Phase::Staging => return Err(DeviceError::Stale),
        }
        Ok(())
    }
}
