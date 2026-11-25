//! Bounded packet transfer through VM-private bounce storage.
//! Ownership/Lock Ordering: exclusive caller ownership; no locks in this module.
//! Invariants: bounded work; malformed queues break before backend packet submission.

use super::*;

impl VirtioNet {
    pub fn service(
        &mut self,
        mem: &mut impl GuestMemory,
        backend: &mut impl NetBackend,
        budget: usize,
    ) {
        self.transfer(mem, backend, budget, &[1, 0]);
    }
    pub fn service_receive(
        &mut self,
        mem: &mut impl GuestMemory,
        backend: &mut impl NetBackend,
        budget: usize,
    ) {
        self.transfer(mem, backend, budget, &[0]);
    }
    fn transfer(
        &mut self,
        mem: &mut impl GuestMemory,
        backend: &mut impl NetBackend,
        budget: usize,
        directions: &[usize],
    ) {
        if self.status & (OK | BROKEN | FAILED) != OK || budget == 0 {
            return;
        }
        if !self.valid(mem) {
            self.broken();
            return;
        }
        // A fixed bound per direction prevents an always-ready TX guest from
        // starving receive or the host terminal. No formatting/allocation here.
        for &direction in directions {
            if !self.queues[direction].ready {
                continue;
            }
            for _ in 0..budget.min(16) {
                match self.one(mem, backend, direction) {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(()) => {
                        self.broken();
                        return;
                    }
                }
            }
        }
    }
    fn one(
        &mut self,
        mem: &mut impl GuestMemory,
        backend: &mut impl NetBackend,
        direction: usize,
    ) -> Result<bool, ()> {
        let Some(head) = self.queues[direction].peek(mem)? else {
            return Ok(false);
        };
        let chain = self.chain(mem, direction, head).ok_or(())?;
        let used = if direction == 1 {
            if !(HEADER + 14..=HEADER + FRAME_MAX).contains(&chain.len) {
                return Err(());
            }
            if !chain.transfer(mem, &mut self.bounce[..chain.len], false) {
                return Err(());
            }
            let h = &self.bounce[..HEADER];
            // num_buffers is unused on TX. Linux does not initialize that
            // field without MRG_RXBUF; it cannot affect our frame/SG bounds.
            if h[0] != 0 || h[1] != 0 {
                return Err(());
            }
            match backend.transmit(&self.bounce[HEADER..chain.len]) {
                Ok(()) => {}
                Err(NetError::Busy) => return Ok(false),
                Err(NetError::Fault) => return Err(()),
            }
            self.tx_packets = self.tx_packets.wrapping_add(1);
            0 // TX buffers are read-only; there is no writable completion data.
        } else {
            if chain.len < HEADER + FRAME_MAX {
                return Err(());
            }
            let count = match backend.receive(&mut self.bounce[HEADER..]) {
                Ok(Some(n)) if (14..=FRAME_MAX).contains(&n) => n,
                Ok(None) | Err(NetError::Busy) => return Ok(false),
                _ => return Err(()),
            };
            self.bounce[..HEADER].fill(0);
            self.bounce[10..12].copy_from_slice(&1u16.to_le_bytes());
            if !chain.transfer(mem, &mut self.bounce[..HEADER + count], true) {
                return Err(());
            }
            self.rx_packets = self.rx_packets.wrapping_add(1);
            (HEADER + count) as u32
        };
        if self.queues[direction].complete(mem, head, used)? {
            self.interrupts |= IRQ_USED;
        }
        Ok(true)
    }
}
