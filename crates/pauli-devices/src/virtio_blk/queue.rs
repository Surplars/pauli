//! Bounded descriptor capture and non-aliasing ring validation.
//! Ownership/Lock Ordering: exclusive caller ownership; no locks in this module.
//! Invariants: cycles, foreign RAM and ring/payload aliases fail before backend I/O.

use super::*;

impl<B: AsMut<[u8]>> VirtioBlock<B> {
    pub(super) fn queue_ranges(&self) -> [(u64, usize, bool); 3] {
        let count = usize::from(self.queue_num);
        let event_bytes = if self.event_idx() { 2 } else { 0 };
        [
            (self.descriptors, 16 * count, false),
            (self.available, 4 + 2 * count + event_bytes, false),
            (self.used, 4 + 8 * count + event_bytes, true),
        ]
    }

    pub(super) fn queue_valid(&self, memory: &impl GuestMemory) -> bool {
        let areas = self.queue_ranges();
        for (index, &(address, len, write)) in areas.iter().enumerate() {
            if !checked(memory, address, len, write) {
                return false;
            }
            for &(other, other_len, _) in &areas[..index] {
                if overlaps(address, len, other, other_len) {
                    return false;
                }
            }
        }
        true
    }

    pub(super) fn chain(&self, memory: &impl GuestMemory, head: u16) -> Option<Chain> {
        let mut chain = Chain::new();
        let mut index = head;
        let mut seen = 0u64;
        let mut writable_seen = false;
        for _ in 0..self.queue_num {
            if index >= self.queue_num || seen & (1u64 << index) != 0 {
                return None;
            }
            seen |= 1u64 << index;
            let mut raw = [0u8; 16];
            if !memory.read(self.descriptors + u64::from(index) * 16, &mut raw) {
                return None;
            }
            let address = le64(&raw, 0);
            let len = le32(&raw, 8) as usize;
            let flags = le16(&raw, 12);
            let write = flags & WRITE != 0;
            if flags & !(NEXT | WRITE) != 0
                || (writable_seen && !write)
                || len > MAX_TRANSFER + 17
                || !checked(memory, address, len, write)
            {
                return None;
            }
            for (base, size, _) in self.queue_ranges() {
                if overlaps(address, len, base, size) {
                    return None;
                }
            }
            writable_seen |= write;
            if write {
                chain.writable += len;
            } else {
                chain.readable += len;
            }
            if chain.readable + chain.writable > MAX_TRANSFER + 17 {
                return None;
            }
            chain.segments[chain.count] = Segment {
                address,
                len,
                write,
            };
            chain.count += 1;
            if flags & NEXT == 0 {
                return (chain.readable >= 16 && chain.writable >= 1).then_some(chain);
            }
            index = le16(&raw, 14);
        }
        None // bounded traversal also rejects cycles and unterminated chains
    }
}
