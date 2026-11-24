//! Bounded slot progress, flush barriers and release-ordered batch publication.
//! Ownership/Lock Ordering: exclusive caller ownership; no locks in this module.
//! Invariants: flush runs alone; release-publish used entries before suppression/IRQ.

use super::*;

impl<B: AsMut<[u8]>> VirtioBlock<B> {
    /// Returns the number of completed requests, including ordinary I/O errors.
    /// Malformed queue/memory state sets NEEDS_RESET and stops all consumption.
    /// A remaining notified batch is resumed by the next service call, so a
    /// guest cannot force unbounded work with one kick or lose work at the budget.
    pub fn service(
        &mut self,
        memory: &mut impl GuestMemory,
        backend: &mut impl BlockBackend,
        budget: usize,
    ) -> usize {
        if budget == 0 {
            return 0;
        }
        let stride = self.buffer.as_mut().unwrap().as_mut().len() / MAX_INFLIGHT;
        if self.draining {
            let mut drained = true;
            for slot in 0..MAX_INFLIGHT {
                let scratch =
                    &mut self.buffer.as_mut().unwrap().as_mut()[slot * stride..(slot + 1) * stride];
                drained &= backend.drain(slot, scratch).is_ready();
            }
            self.draining = !drained;
            if !drained {
                return 0;
            }
        }
        if !self.needs_service() {
            return 0;
        }
        if !self.queue_valid(memory) {
            self.broken();
            return 0;
        }
        let mut header = [0u8; 4];
        if !memory.read(self.available, &mut header) {
            self.broken();
            return 0;
        }
        let flags = le16(&header, 0);
        let available = le16(&header, 2);
        fence(Ordering::Acquire);
        if flags & !1 != 0
            || usize::from(available.wrapping_sub(self.last_available)) + self.inflight
                > usize::from(self.queue_num)
        {
            self.broken();
            return 0;
        }
        let first_used = self.used_index;
        let work = self.poll_slots(budget.min(MAX_SERVICE_REQUESTS), stride, memory, backend);
        self.admit_requests(work, stride, available, memory, backend);
        if self.last_available == available {
            self.notified = false;
        }
        let completed = usize::from(self.used_index.wrapping_sub(first_used));
        if completed != 0 {
            // Publish all status/data/used elements together, then raise one
            // level. No delay timer: a lone completion is published immediately.
            fence(Ordering::Release);
            if !memory.write(self.used + 2, &self.used_index.to_le_bytes()) {
                self.broken();
                return 0;
            }
            self.completed = self.completed.wrapping_add(completed as u64);
            self.completion_batches += 1;
            self.largest_batch = self.largest_batch.max(completed);
            if self.notify_used(memory, first_used).is_none() {
                self.broken();
                return completed;
            }
        }
        if self.status & NEEDS_RESET == 0
            && !self.notified
            && self.event_idx()
            && self.rearm_available(memory).is_none()
        {
            self.broken();
        }
        completed
    }

    /// Finish one slot without publishing used.idx. Guest state is captured
    /// at admission; native handles and staging remain private to this slot.
    pub(super) fn progress(
        &mut self,
        slot: usize,
        stride: usize,
        memory: &mut impl GuestMemory,
        backend: &mut impl BlockBackend,
    ) -> Option<()> {
        let scratch =
            &mut self.buffer.as_mut().unwrap().as_mut()[slot * stride..(slot + 1) * stride];
        let written = match self.pending[slot]
            .as_mut()
            .unwrap()
            .progress(slot, memory, backend, scratch)?
        {
            Poll::Pending => return Some(()),
            Poll::Ready(written) => written,
        };
        let request = self.pending[slot].take().unwrap();
        self.inflight -= 1;
        if self.barrier == Some(slot) {
            self.barrier = None;
        }
        let ring = u64::from(self.used_index & (self.queue_num - 1));
        let mut element = [0u8; 8];
        element[..4].copy_from_slice(&u32::from(request.head).to_le_bytes());
        element[4..].copy_from_slice(&written.to_le_bytes());
        if !memory.write(self.used + 4 + ring * 8, &element) {
            return None;
        }
        self.used_index = self.used_index.wrapping_add(1);
        if request.kind == 0 {
            self.read_bytes = self.read_bytes.wrapping_add(request.done as u64);
        }
        if request.kind == 1 {
            self.write_bytes = self.write_bytes.wrapping_add(request.done as u64);
        }
        self.flushes = self.flushes.wrapping_add(u64::from(request.flushed));
        Some(())
    }

    fn poll_slots(
        &mut self,
        mut work: usize,
        stride: usize,
        memory: &mut impl GuestMemory,
        backend: &mut impl BlockBackend,
    ) -> usize {
        // Poll each occupied slot once; one slow request cannot hide ready
        // siblings. A barrier is admitted in ring order and runs alone.
        let first = self.next_poll;
        for n in 0..MAX_INFLIGHT {
            let slot = (first + n) % MAX_INFLIGHT;
            if work == 0
                || self.pending[slot].is_none()
                || (self.barrier == Some(slot) && self.inflight != 1)
            {
                continue;
            }
            work -= 1;
            self.next_poll = (slot + 1) % MAX_INFLIGHT;
            if self.progress(slot, stride, memory, backend).is_none() {
                self.broken();
                break;
            }
        }
        work
    }

    fn admit_requests(
        &mut self,
        mut work: usize,
        stride: usize,
        available: u16,
        memory: &mut impl GuestMemory,
        backend: &mut impl BlockBackend,
    ) {
        while work != 0
            && self.last_available != available
            && self.barrier.is_none()
            && self.status & NEEDS_RESET == 0
        {
            let Some(slot) = self.pending.iter().position(Option::is_none) else {
                break;
            };
            let ring = u64::from(self.last_available & (self.queue_num - 1));
            let mut raw_head = [0u8; 2];
            if !memory.read(self.available + 4 + ring * 2, &mut raw_head) {
                self.broken();
                break;
            }
            let head = u16::from_le_bytes(raw_head);
            if self
                .pending
                .iter()
                .flatten()
                .any(|request| request.head == head)
            {
                self.broken();
                break;
            }
            let Some(request) = self
                .chain(memory, head)
                .and_then(|chain| self.request(head, chain, memory))
            else {
                self.broken();
                break;
            };
            if request.barrier() {
                self.barrier = Some(slot);
            }
            self.pending[slot] = Some(request);
            self.inflight += 1;
            self.peak_inflight = self.peak_inflight.max(self.inflight);
            self.last_available = self.last_available.wrapping_add(1);
            self.available_armed = false;
            if self.barrier.is_some() && self.inflight != 1 {
                break;
            }
            work -= 1;
            if self.progress(slot, stride, memory, backend).is_none() {
                self.broken();
                break;
            }
        }
    }
}
