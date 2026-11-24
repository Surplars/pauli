//! Native request ownership, independent of transport access and RTOS completions.
//! Ownership/Lock Ordering: queue lock owns phase/index changes; no I/O/callback here.
//! Invariants: completed/copying data survives recovery of another pending request.
//! Lifecycle: Free -> Staging -> Pending -> Complete -> Copying -> Free;
//! cancelled Pending -> Detached -> Free after DMA retirement only.

use super::*;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Free,
    Staging,
    Pending,
    Complete,
    Copying,
    Detached,
}
#[derive(Clone, Copy)]
pub(super) struct Slot {
    pub generation: u64,
    pub phase: Phase,
    pub request: BlockSubmission,
    pub result: Result<usize>,
}
impl Slot {
    pub const fn new() -> Self {
        Self {
            generation: 0,
            phase: Phase::Free,
            request: BlockSubmission {
                operation: BlockOperation::Read,
                sector: 0,
                bytes: 0,
                deadline: u64::MAX,
                event: None,
            },
            result: Ok(0),
        }
    }
    pub fn finish(&mut self, result: Result<usize>) {
        if self.phase == Phase::Detached {
            self.phase = Phase::Free;
            return;
        }
        self.result = result;
        self.phase = Phase::Complete;
        // Publish inside the slot lock: cancellation/reuse cannot retire an
        // owner while an old producer still owes its completion notification.
        if let Some(event) = self.request.event {
            event.notify();
        }
    }
}
pub(super) struct State {
    pub slots: [Slot; REQUEST_SLOTS],
    pub used: u16,
    pub available: u16,
    pub recovering: bool,
    pub failed: bool,
}
impl State {
    pub const fn new() -> Self {
        Self {
            slots: [Slot::new(); REQUEST_SLOTS],
            used: 0,
            available: 0,
            recovering: false,
            failed: false,
        }
    }
}
