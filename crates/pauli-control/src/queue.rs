//! One bounded request per vCPU channel. The platform wraps this pure state
//! machine in a short lock; it never retains that lock across service execution.
//! Cancellation before take prevents effects. Once taken, cancellation cannot
//! undo an effect, but suppresses completion and never touches a stale context.
//! Ownership/Lock Ordering: caller serializes this queue; release before effects.
//! Invariants: channel generations reject completions from a cancelled/reused slot.
//! Lifecycle: Empty -> Queued -> Executing -> Done -> Empty; cancel any -> Empty.
use crate::{Call, abi::Return};
/// Limit simultaneous control calls, not the VM or vCPU identities that may call.
pub const MAX_PENDING: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket {
    channel: usize,
    index: usize,
    generation: u64,
}
impl Ticket {
    pub const fn channel(self) -> usize {
        self.channel
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Work {
    pub ticket: Ticket,
    pub source: u32,
    pub request: Call,
}
#[derive(Clone, Copy)]
enum Phase {
    Queued,
    Executing,
    Done(Return),
}
#[derive(Clone, Copy)]
struct Slot {
    channel: usize,
    generation: u64,
    source: u32,
    request: Call,
    phase: Phase,
}
impl Slot {
    // The array position is already the ticket index. Store it only in tickets
    // handed out to callers, and reconstruct it when checking a returned ticket.
    fn ticket(&self, index: usize) -> Ticket {
        Ticket {
            channel: self.channel,
            index,
            generation: self.generation,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Busy,
    Invalid,
    Exhausted,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Poll {
    Pending,
    Complete(Return),
    Cancelled,
}

pub struct Queue {
    slots: [Option<Slot>; MAX_PENDING],
    generation: u64,
    next: usize,
}
impl Queue {
    pub const fn new() -> Self {
        Self {
            slots: [None; MAX_PENDING],
            generation: 0,
            next: 0,
        }
    }
    pub fn submit(
        &mut self,
        channel: usize,
        source: u32,
        request: impl Into<Call>,
    ) -> Result<Ticket, Error> {
        if source == 0 {
            return Err(Error::Invalid);
        }
        if self
            .slots
            .iter()
            .flatten()
            .any(|slot| slot.channel == channel)
        {
            return Err(Error::Busy);
        }
        let index = self
            .slots
            .iter()
            .position(Option::is_none)
            .ok_or(Error::Exhausted)?;
        self.generation = self.generation.checked_add(1).ok_or(Error::Exhausted)?;
        let slot = Slot {
            channel,
            generation: self.generation,
            source,
            request: request.into(),
            phase: Phase::Queued,
        };
        let ticket = slot.ticket(index);
        self.slots[index] = Some(slot);
        Ok(ticket)
    }
    pub fn has_work(&self) -> bool {
        self.slots
            .iter()
            .flatten()
            .any(|slot| matches!(slot.phase, Phase::Queued))
    }
    pub fn take(&mut self) -> Option<Work> {
        for offset in 0..MAX_PENDING {
            let index = (self.next + offset) % MAX_PENDING;
            if let Some(slot) = self.slots[index]
                .as_mut()
                .filter(|slot| matches!(slot.phase, Phase::Queued))
            {
                slot.phase = Phase::Executing;
                self.next = (index + 1) % MAX_PENDING;
                return Some(Work {
                    ticket: slot.ticket(index),
                    source: slot.source,
                    request: slot.request,
                });
            }
        }
        None
    }
    pub fn complete(&mut self, work: Work, result: Return) -> bool {
        let Some(slot) = self.slots[work.ticket.index].as_mut().filter(|slot| {
            slot.ticket(work.ticket.index) == work.ticket && matches!(slot.phase, Phase::Executing)
        }) else {
            return false;
        };
        slot.phase = Phase::Done(result);
        true
    }
    pub fn poll(&mut self, ticket: Ticket) -> Poll {
        self.poll_operation(ticket).0
    }
    /// Return the original operation only to the consumer of this generation's completed reply.
    pub fn poll_operation(&mut self, ticket: Ticket) -> (Poll, Option<crate::Operation>) {
        let Some(slot) = self.slots[ticket.index]
            .as_ref()
            .filter(|slot| slot.ticket(ticket.index) == ticket)
        else {
            return (Poll::Cancelled, None);
        };
        if let Phase::Done(result) = slot.phase {
            let operation = slot.request.vm_operation();
            self.slots[ticket.index] = None;
            (Poll::Complete(result), operation)
        } else {
            (Poll::Pending, None)
        }
    }
    pub fn cancel(&mut self, ticket: Ticket) {
        if self.slots[ticket.index].is_some_and(|slot| slot.ticket(ticket.index) == ticket) {
            self.slots[ticket.index] = None;
        }
    }
}
