//! Bounded used-ring collection; no formatting, callbacks, guest access or large copies.
//! Ownership/Lock Ordering: short native queue lock; completion hints are atomic stores only.
//! Invariants: Acquire pairs with device publication; IDs/status/length are checked before reuse.

use super::*;
impl<I: LocalIrq> VirtioBlk<I> {
    pub(super) fn service_used(&self) -> Result<usize> {
        let mut state = self.state.lock();
        if state.recovering {
            return Err(DeviceError::NotReady);
        }
        if state.failed {
            return Err(DeviceError::Fault);
        }
        let result = self.collect_used(&mut state);
        if result.is_err() {
            state.failed = true;
            for slot in &state.slots {
                if let Some(event) = slot.request.event {
                    event.notify();
                }
            }
        }
        result
    }

    fn collect_used(&self, state: &mut State) -> Result<usize> {
        self.dma.for_cpu(
            USED_OFFSET,
            core::mem::size_of::<VirtqUsed<QUEUE_SIZE>>(),
            Direction::FromDevice,
        )?;
        // SAFETY: used.idx is an aligned volatile device-owned word inside pinned rings.
        let end = u16::from_le(unsafe { core::ptr::addr_of!((*self.used()).idx).read_volatile() });
        fence(Ordering::Acquire);
        if end.wrapping_sub(state.used) as usize > REQUEST_SLOTS {
            return Err(DeviceError::Fault);
        }
        let mut completed = 0;
        while state.used != end {
            let index = state.used as usize % QUEUE_SIZE;
            // SAFETY: ring index is bounded; Acquire/cache sync made the element visible.
            let element =
                unsafe { core::ptr::addr_of!((*self.used()).ring[index]).read_volatile() };
            let head = u32::from_le(element.id) as usize;
            if head % CHAIN_SIZE != 0 || head / CHAIN_SIZE >= REQUEST_SLOTS {
                return Err(DeviceError::Fault);
            }
            let index = head / CHAIN_SIZE;
            let slot = &mut state.slots[index];
            if !matches!(slot.phase, Phase::Pending | Phase::Detached) {
                return Err(DeviceError::Fault);
            }
            let base = Self::request_offset(index);
            self.dma
                .for_cpu(base + STATUS_OFFSET, 1, Direction::FromDevice)?;
            // SAFETY: this request was returned by the device; fixed status byte fits its slot.
            let status = unsafe { self.dma.pointer(base + STATUS_OFFSET).read_volatile() };
            let length = u32::from_le(element.len) as usize;
            let expected = if slot.request.operation == BlockOperation::Read {
                slot.request.bytes + 1
            } else {
                1
            };
            if length > expected
                || (status == STATUS_OK
                    && slot.request.operation == BlockOperation::Read
                    && length != expected)
            {
                return Err(DeviceError::Fault);
            }
            let result = if status == STATUS_OK {
                Ok(slot.request.bytes)
            } else {
                Err(DeviceError::Fault)
            };
            slot.finish(result);
            state.used = state.used.wrapping_add(1);
            completed += 1;
        }
        Ok(completed)
    }
}
