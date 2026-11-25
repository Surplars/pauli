//! Network split rings and bounded descriptor stream capture.
//! Ownership/Lock Ordering: exclusive caller ownership; no locks in this module.
//! Invariants: disjoint rings, bounded chains and payload checks precede copied I/O.

use super::*;

#[derive(Clone, Copy)]
pub(super) struct Queue {
    pub(super) num: u16,
    pub(super) desc: u64,
    pub(super) avail: u64,
    pub(super) used: u64,
    pub(super) ready: bool,
    next: u16,
    used_idx: u16,
}
impl Queue {
    pub(super) const fn new() -> Self {
        Self {
            num: 0,
            desc: 0,
            avail: 0,
            used: 0,
            ready: false,
            next: 0,
            used_idx: 0,
        }
    }
    pub(super) fn areas(&self) -> [(u64, usize, bool); 3] {
        let n = usize::from(self.num);
        [
            (self.desc, 16 * n, false),
            (self.avail, 6 + 2 * n, false),
            (self.used, 6 + 8 * n, true),
        ]
    }
    pub(super) fn peek(&self, mem: &impl GuestMemory) -> Result<Option<u16>, ()> {
        let mut raw = [0; 4];
        if !mem.read(self.avail, &mut raw) {
            return Err(());
        }
        let idx = le16(&raw, 2);
        fence(Ordering::Acquire);
        if le16(&raw, 0) & !1 != 0 || idx.wrapping_sub(self.next) > self.num {
            return Err(());
        }
        if idx == self.next {
            return Ok(None);
        }
        let mut head = [0; 2];
        if !mem.read(
            self.avail + 4 + 2 * u64::from(self.next & (self.num - 1)),
            &mut head,
        ) {
            return Err(());
        }
        Ok(Some(u16::from_le_bytes(head)))
    }
    pub(super) fn complete(
        &mut self,
        mem: &mut impl GuestMemory,
        head: u16,
        len: u32,
    ) -> Result<bool, ()> {
        let mut raw = [0; 8];
        raw[..4].copy_from_slice(&u32::from(head).to_le_bytes());
        raw[4..].copy_from_slice(&len.to_le_bytes());
        if !mem.write(
            self.used + 4 + 8 * u64::from(self.used_idx & (self.num - 1)),
            &raw,
        ) {
            return Err(());
        }
        self.used_idx = self.used_idx.wrapping_add(1);
        fence(Ordering::Release);
        if !mem.write(self.used + 2, &self.used_idx.to_le_bytes()) {
            return Err(());
        }
        self.next = self.next.wrapping_add(1);
        // The driver arms callbacks then rechecks used.idx. Publish the index
        // before reading suppression so either side observes the other's store.
        fence(Ordering::SeqCst);
        let mut flags = [0; 2];
        if !mem.read(self.avail, &mut flags) {
            return Err(());
        }
        Ok(u16::from_le_bytes(flags) & 1 == 0)
    }
}
#[derive(Clone, Copy)]
struct Segment {
    addr: u64,
    len: usize,
}
pub(super) struct Chain {
    segments: [Segment; 64],
    count: usize,
    pub(super) len: usize,
}
impl Chain {
    pub(super) fn transfer(
        &self,
        mem: &mut impl GuestMemory,
        data: &mut [u8],
        write: bool,
    ) -> bool {
        let mut done = 0;
        for s in &self.segments[..self.count] {
            let n = s.len.min(data.len() - done);
            if n != 0 {
                let ok = if write {
                    mem.write(s.addr, &data[done..done + n])
                } else {
                    mem.read(s.addr, &mut data[done..done + n])
                };
                if !ok {
                    return false;
                }
                done += n;
            }
            if done == data.len() {
                return true;
            }
        }
        false
    }
}

impl VirtioNet {
    pub(super) fn areas(&self) -> impl Iterator<Item = (u64, usize, bool)> + '_ {
        self.queues
            .iter()
            .filter(|q| q.ready)
            .flat_map(Queue::areas)
    }
    pub(super) fn valid(&self, mem: &impl GuestMemory) -> bool {
        for (i, (addr, len, write)) in self.areas().enumerate() {
            if addr.checked_add(len as u64).is_none() || !mem.check(addr, len, write) {
                return false;
            }
            for (other, size, _) in self.areas().take(i) {
                if overlap(addr, len, other, size) {
                    return false;
                }
            }
        }
        true
    }
    pub(super) fn chain(&self, mem: &impl GuestMemory, queue: usize, head: u16) -> Option<Chain> {
        let q = &self.queues[queue];
        let mut chain = Chain {
            segments: [Segment { addr: 0, len: 0 }; 64],
            count: 0,
            len: 0,
        };
        let mut seen = 0u64;
        let mut index = head;
        for _ in 0..q.num {
            if index >= q.num || seen & (1u64 << index) != 0 {
                return None;
            }
            seen |= 1u64 << index;
            let mut raw = [0; 16];
            if !mem.read(q.desc + 16 * u64::from(index), &mut raw) {
                return None;
            }
            let addr = le64(&raw, 0);
            let len = le32(&raw, 8) as usize;
            let flags = le16(&raw, 12);
            let write = queue == 0;
            if flags & !3 != 0
                || (flags & 2 != 0) != write
                || addr.checked_add(len as u64).is_none()
                || (len != 0 && !mem.check(addr, len, write))
            {
                return None;
            }
            for (base, size, _) in self.areas() {
                if overlap(addr, len, base, size) {
                    return None;
                }
            }
            chain.len = chain.len.checked_add(len)?;
            chain.segments[chain.count] = Segment { addr, len };
            chain.count += 1;
            if flags & 1 == 0 {
                return Some(chain);
            }
            index = le16(&raw, 14);
        }
        None
    }
}
