//! Native packet copies, never physical DMA into a guest/caller frame.
//! Ownership/Lock Ordering: one native queue lock serializes small bounded frame copies.
//! Invariants: validate native IDs/lengths before touching payload; rearm RX after copying.

use super::*;
impl<I: LocalIrq> VirtioNet<I> {
    pub(super) fn send(&self, frame: &[u8]) -> Result<()> {
        if !(14..=FRAME).contains(&frame.len()) {
            return Err(DeviceError::InvalidResource);
        }
        let mut state = self.state.lock();
        state.ready()?;
        if let Err(error) = self.reap_tx(&mut state) {
            state.failed = true;
            return Err(error);
        }
        let id = state
            .tx_busy
            .iter()
            .position(|busy| !busy)
            .ok_or(DeviceError::Busy)?;
        let offset = Self::data(TX, id);
        // SAFETY: this idle slot is CPU-owned; checked frame length fits its private buffer.
        unsafe {
            let bytes = self.dma.pointer(offset);
            core::ptr::write_bytes(bytes, 0, HEADER);
            core::ptr::copy_nonoverlapping(frame.as_ptr(), bytes.add(HEADER), frame.len());
            core::ptr::addr_of_mut!((*self.desc(TX).add(id)).len)
                .write_volatile(((HEADER + frame.len()) as u32).to_le());
        }
        self.dma
            .for_device(offset, HEADER + frame.len(), Direction::ToDevice)?;
        self.dma
            .for_device(TX * QUEUE_BYTES + id * 16, 16, Direction::ToDevice)?;
        state.tx_busy[id] = true;
        if let Err(error) = self.publish(TX, id, state.tx_available) {
            state.failed = true;
            return Err(error);
        }
        state.tx_available = state.tx_available.wrapping_add(1);
        Ok(())
    }
    pub(super) fn recv(&self, out: &mut [u8]) -> Result<Option<usize>> {
        if out.len() < FRAME {
            return Err(DeviceError::InvalidResource);
        }
        let mut state = self.state.lock();
        state.ready()?;
        self.dma.for_cpu(
            USED,
            core::mem::size_of::<VirtqUsed<N>>(),
            Direction::FromDevice,
        )?;
        // SAFETY: fixed used.idx in the native RX queue, synchronized for CPU reads.
        let end =
            u16::from_le(unsafe { core::ptr::addr_of!((*self.used(RX)).idx).read_volatile() });
        fence(Ordering::Acquire);
        let pending = end.wrapping_sub(state.rx_used) as usize;
        if pending == 0 {
            return Ok(None);
        }
        if pending > N {
            state.failed = true;
            return Err(DeviceError::Fault);
        }
        // SAFETY: bounded ring index and Acquire publication establish this element's visibility.
        let element = unsafe {
            core::ptr::addr_of!((*self.used(RX)).ring[state.rx_used as usize % N]).read_volatile()
        };
        let id = u32::from_le(element.id) as usize;
        let len = u32::from_le(element.len) as usize;
        if id >= N || !(HEADER..=BUFFER).contains(&len) {
            state.failed = true;
            return Err(DeviceError::Fault);
        }
        let offset = Self::data(RX, id);
        self.dma.for_cpu(offset, len, Direction::FromDevice)?;
        // SAFETY: returned descriptor grants CPU ownership; validated id/len fit private storage.
        let packet = unsafe { core::slice::from_raw_parts(self.dma.pointer(offset), len) };
        let count = frame::rx_payload(packet).map(|payload| {
            out[..payload.len()].copy_from_slice(payload);
            payload.len()
        });
        self.dma.for_device(offset, BUFFER, Direction::FromDevice)?;
        if let Err(error) = self.publish(RX, id, state.rx_available) {
            state.failed = true;
            return Err(error);
        }
        state.rx_available = state.rx_available.wrapping_add(1);
        state.rx_used = state.rx_used.wrapping_add(1);
        Ok(count)
    }
}
