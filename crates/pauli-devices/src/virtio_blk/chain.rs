//! Captured descriptor streams; framing is independent of descriptor boundaries.
//! Ownership/Lock Ordering: exclusive caller ownership; no locks in this module.
//! Invariants: readable/writable streams retain descriptor direction; copies recheck RAM.

use super::*;

impl Chain {
    pub(super) fn new() -> Self {
        Self {
            segments: [EMPTY_SEGMENT; MAX_QUEUE_SIZE as usize],
            count: 0,
            readable: 0,
            writable: 0,
        }
    }

    /// Modern VirtIO framing is independent of descriptor boundaries. Header,
    /// payload and status may span descriptors or share a descriptor.
    pub(super) fn read(&self, memory: &impl GuestMemory, offset: usize, out: &mut [u8]) -> bool {
        self.read_with(offset, out, |gpa, out| memory.read(gpa, out))
    }

    pub(super) fn read_payload(
        &self,
        memory: &impl GuestMemory,
        offset: usize,
        out: &mut [u8],
    ) -> bool {
        self.read_with(offset, out, |gpa, out| memory.read_payload(gpa, out))
    }

    pub(super) fn read_with(
        &self,
        mut offset: usize,
        out: &mut [u8],
        mut read: impl FnMut(u64, &mut [u8]) -> bool,
    ) -> bool {
        let mut copied = 0;
        for segment in &self.segments[..self.count] {
            if segment.write {
                continue;
            }
            if offset >= segment.len {
                offset -= segment.len;
                continue;
            }
            let count = (segment.len - offset).min(out.len() - copied);
            if !read(
                segment.address + offset as u64,
                &mut out[copied..copied + count],
            ) {
                return false;
            }
            copied += count;
            if copied == out.len() {
                return true;
            }
            offset = 0;
        }
        false
    }

    pub(super) fn write(&self, memory: &mut impl GuestMemory, offset: usize, input: &[u8]) -> bool {
        self.write_with(offset, input, |gpa, data| memory.write(gpa, data))
    }

    pub(super) fn write_payload(
        &self,
        memory: &mut impl GuestMemory,
        offset: usize,
        input: &[u8],
    ) -> bool {
        self.write_with(offset, input, |gpa, data| memory.write_payload(gpa, data))
    }

    pub(super) fn write_with(
        &self,
        mut offset: usize,
        input: &[u8],
        mut write: impl FnMut(u64, &[u8]) -> bool,
    ) -> bool {
        let mut copied = 0;
        for segment in &self.segments[..self.count] {
            if !segment.write {
                continue;
            }
            if offset >= segment.len {
                offset -= segment.len;
                continue;
            }
            let count = (segment.len - offset).min(input.len() - copied);
            if !write(
                segment.address + offset as u64,
                &input[copied..copied + count],
            ) {
                return false;
            }
            copied += count;
            if copied == input.len() {
                return true;
            }
            offset = 0;
        }
        false
    }

    pub(super) fn data_limits(&self, write: bool, mut offset: usize, mut len: usize) -> bool {
        let mut segments = 0;
        for segment in &self.segments[..self.count] {
            if segment.write != write {
                continue;
            }
            if offset >= segment.len {
                offset -= segment.len;
                continue;
            }
            let count = (segment.len - offset).min(len);
            if count != 0 {
                segments += 1;
            }
            if count > MAX_SEGMENT_SIZE || segments > MAX_SEGMENTS {
                return false;
            }
            len -= count;
            if len == 0 {
                return true;
            }
            offset = 0;
        }
        len == 0
    }
}
