//! Native queue setup/recovery, preserving CPU-owned completed bounce payloads.
//! Ownership/Lock Ordering: dispatcher serializes recovery; IRQ-off work is bounded.
//! Invariants: reset ACK precedes descriptor reuse; request payloads are not zeroed on recovery.

use super::*;

impl<I: LocalIrq> VirtioBlk<I> {
    pub(super) fn configure_queue(&self) -> Result<()> {
        // SAFETY: initialization or acknowledged reset grants CPU ownership of rings.
        unsafe {
            core::ptr::write_bytes(self.dma.pointer(0), 0, REQUEST_OFFSET);
        }
        for slot in 0..REQUEST_SLOTS {
            let head = slot * CHAIN_SIZE;
            let address = self.dma.device_address(Self::request_offset(slot))?;
            let chain = [
                VirtqDesc {
                    addr: address.to_le(),
                    len: (HEADER_BYTES as u32).to_le(),
                    flags: desc_flags::NEXT.to_le(),
                    next: ((head + 1) as u16).to_le(),
                },
                VirtqDesc {
                    addr: (address + HEADER_BYTES as u64).to_le(),
                    len: 0,
                    flags: desc_flags::NEXT.to_le(),
                    next: ((head + 2) as u16).to_le(),
                },
                VirtqDesc {
                    addr: (address + STATUS_OFFSET as u64).to_le(),
                    len: 1u32.to_le(),
                    flags: desc_flags::WRITE.to_le(),
                    next: 0,
                },
            ];
            for (index, descriptor) in chain.into_iter().enumerate() {
                // SAFETY: fixed slot/head ranges fit QUEUE_SIZE; device is reset.
                unsafe {
                    self.descriptor()
                        .add(head + index)
                        .write_volatile(descriptor);
                }
            }
        }
        self.dma
            .for_device(0, REQUEST_OFFSET, Direction::Bidirectional)?;
        self.transport.setup_queue(
            0,
            QUEUE_SIZE,
            self.dma.device_address(0)?,
            self.dma.device_address(AVAIL_OFFSET)?,
            self.dma.device_address(USED_OFFSET)?,
        )
    }

    pub(super) fn recover(&self, reason: DeviceError) -> Result<()> {
        {
            let mut state = self.state.lock();
            if state.recovering {
                return Err(DeviceError::Busy);
            }
            state.recovering = true;
        }
        // Interrupts stay enabled during bounded MMIO reset polling. New submit
        // and IRQ collection see recovering, so neither can touch retired rings.
        let reset = self.transport.reset_and_wait();
        let mut state = self.state.lock();
        if let Err(error) = reset {
            state.failed = true;
            state.recovering = false;
            // DMA ownership is unknown: quarantine all pending storage forever.
            for slot in &state.slots {
                if let Some(event) = slot.request.event {
                    event.notify();
                }
            }
            return Err(error);
        }
        for slot in &mut state.slots {
            if matches!(slot.phase, Phase::Pending | Phase::Detached) {
                slot.finish(Err(reason));
            }
        }
        let configured = self.reconfigure();
        state.used = 0;
        state.available = 0;
        state.failed = configured.is_err();
        state.recovering = false;
        configured
    }

    fn reconfigure(&self) -> Result<()> {
        self.transport.validate_device(DEVICE_ID_BLOCK)?;
        self.transport
            .set_status_bits(status::ACKNOWLEDGE | status::DRIVER);
        if self.transport.negotiate_features(REQUESTED)?.lo != self.features {
            return Err(DeviceError::Fault);
        }
        self.configure_queue()?;
        self.transport.set_status_bits(status::DRIVER_OK);
        self.transport.ack_interrupts();
        Ok(())
    }
}
