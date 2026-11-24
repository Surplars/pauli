//! Native queue setup, TX reaping and reset-safe ownership transitions.
//! Ownership/Lock Ordering: short queue lock owns indices; bounded reset polling is unlocked.
//! Invariants: no clearing or reusing device-owned buffers until reset acknowledgement.

use super::*;
impl<I: LocalIrq> VirtioNet<I> {
    pub(super) fn configure(&self) -> Result<()> {
        // SAFETY: initial/reset acknowledgement grants CPU ownership of this private pool.
        unsafe {
            core::ptr::write_bytes(self.dma.pointer(0), 0, DMA_BYTES);
        }
        for queue in [RX, TX] {
            for id in 0..N {
                let descriptor = VirtqDesc {
                    addr: self.dma.device_address(Self::data(queue, id))?.to_le(),
                    len: if queue == RX {
                        (BUFFER as u32).to_le()
                    } else {
                        0
                    },
                    flags: if queue == RX {
                        desc_flags::WRITE.to_le()
                    } else {
                        0
                    },
                    next: 0,
                };
                // SAFETY: fixed queue/id ranges fit the CPU-owned descriptor/avail areas.
                unsafe {
                    self.desc(queue).add(id).write_volatile(descriptor);
                    if queue == RX {
                        core::ptr::addr_of_mut!((*self.available(queue)).ring[id])
                            .write_volatile((id as u16).to_le());
                        core::ptr::addr_of_mut!((*self.available(queue)).idx)
                            .write_volatile((N as u16).to_le());
                    }
                }
            }
            let base = queue * QUEUE_BYTES;
            self.transport.setup_queue(
                queue as u32,
                N,
                self.dma.device_address(base)?,
                self.dma.device_address(base + AVAIL)?,
                self.dma.device_address(base + USED)?,
            )?;
        }
        self.dma
            .for_device(0, DMA_BYTES, Direction::Bidirectional)?;
        self.transport.set_status_bits(status::DRIVER_OK);
        self.transport.notify_queue(RX as u32);
        Ok(())
    }
    pub(super) fn reset(&self, restart: bool) -> Result<()> {
        {
            let mut state = self.state.lock();
            if state.resetting {
                return Err(DeviceError::Busy);
            }
            state.resetting = true;
            state.running = false;
        }
        let stopped = self.transport.reset_and_wait();
        let mut state = self.state.lock();
        if let Err(error) = stopped {
            state.failed = true;
            state.resetting = false;
            return Err(error);
        }
        if restart {
            self.transport
                .set_status_bits(status::ACKNOWLEDGE | status::DRIVER);
            let result = self
                .transport
                .negotiate_features(REQUESTED)
                .and_then(|selected| {
                    if selected.lo != self.features {
                        return Err(DeviceError::Fault);
                    }
                    self.configure()
                });
            if let Err(error) = result {
                state.failed = true;
                state.resetting = false;
                return Err(error);
            }
        } else {
            // SAFETY: reset ACK stops DMA; running=false excludes every CPU packet operation.
            unsafe {
                core::ptr::write_bytes(self.dma.pointer(0), 0, DMA_BYTES);
            }
        }
        self.transport.ack_interrupts();
        *state = State::new(restart);
        Ok(())
    }
    pub(super) fn reap_tx(&self, state: &mut State) -> Result<()> {
        self.dma.for_cpu(
            TX * QUEUE_BYTES + USED,
            core::mem::size_of::<VirtqUsed<N>>(),
            Direction::FromDevice,
        )?;
        // SAFETY: volatile used.idx lies inside the mapped private queue page.
        let end =
            u16::from_le(unsafe { core::ptr::addr_of!((*self.used(TX)).idx).read_volatile() });
        fence(Ordering::Acquire);
        if end.wrapping_sub(state.tx_used) as usize > N {
            return Err(DeviceError::Fault);
        }
        while state.tx_used != end {
            // SAFETY: bounded ring index, acquired from the device's used.idx.
            let id = u32::from_le(unsafe {
                core::ptr::addr_of!((*self.used(TX)).ring[state.tx_used as usize % N].id)
                    .read_volatile()
            }) as usize;
            if id >= N || !state.tx_busy[id] {
                return Err(DeviceError::Fault);
            }
            state.tx_busy[id] = false;
            state.tx_used = state.tx_used.wrapping_add(1);
        }
        Ok(())
    }
    pub(super) fn publish(&self, queue: usize, id: usize, next: u16) -> Result<()> {
        // SAFETY: queue owner holds the lock; bounded indices name CPU-owned avail fields.
        unsafe {
            core::ptr::addr_of_mut!((*self.available(queue)).ring[next as usize % N])
                .write_volatile((id as u16).to_le());
            fence(Ordering::Release);
            core::ptr::addr_of_mut!((*self.available(queue)).idx)
                .write_volatile(next.wrapping_add(1).to_le());
        }
        self.dma.for_device(
            queue * QUEUE_BYTES + AVAIL,
            core::mem::size_of::<VirtqAvail<N>>(),
            Direction::ToDevice,
        )?;
        self.transport.notify_queue(queue as u32);
        Ok(())
    }
}
