//! Selftest-only observations of the first fence generations of one target epoch.
//! The existing mailbox guard serializes producer, owner and report; no new lock.
//! Never assign a shared batch's timestamp to an individual fence.
use super::FenceTicket;
use core::cell::Cell;
use pauli_core::arch::FENCE_SAMPLE_LIMIT as LIMIT;

const ABSENT: u8 = u8::MAX; // A requester fits in a native-word waiter mask.
const _: () = assert!(LIMIT <= u32::BITS as usize);

#[derive(Clone, Copy, PartialEq)]
enum Window {
    Open,
    Full,
    Wrapped,
}

struct Kind {
    window: Cell<Window>,
    requester: [Cell<u8>; LIMIT],
    ticks: [Cell<u64>; LIMIT],
    classified: Cell<u32>,
    deferred: Cell<u32>,
}
impl Kind {
    const fn new() -> Self {
        Self {
            window: Cell::new(Window::Open),
            requester: [const { Cell::new(ABSENT) }; LIMIT],
            ticks: [const { Cell::new(0) }; LIMIT],
            classified: Cell::new(0),
            deferred: Cell::new(0),
        }
    }
}

pub(super) struct Samples([Kind; 2]);
impl Samples {
    pub(super) const fn new() -> Self {
        Self([const { Kind::new() }; 2])
    }

    pub(super) fn record(
        &self,
        prior: [u64; 2],
        next: [u64; 2],
        notify: usize,
        clock: impl Fn() -> u64,
    ) {
        for (i, kind) in self.0.iter().enumerate() {
            if next[i] < prior[i] {
                // A new low-numbered ticket must not classify a first-window sample.
                kind.window.set(Window::Wrapped);
            }
            if prior[i] == next[i] || kind.window.get() != Window::Open {
                continue;
            }
            // Close even on skipped batches: a later counter wrap is not a
            // second chance to fill a hole in the first generation window.
            if next[i] >= LIMIT as u64 {
                kind.window.set(Window::Full);
            }
            if prior[1 - i] != next[1 - i]
                || notify.count_ones() != 1
                || prior[i].checked_add(1) != Some(next[i])
                || next[i] > LIMIT as u64
            {
                continue;
            }
            let at = next[i] as usize - 1;
            kind.ticks[at].set(clock());
            kind.requester[at].set(notify.trailing_zeros() as u8);
        }
    }

    pub(super) fn get(&self, instruction: bool, generation: usize) -> Option<(u8, u64)> {
        let kind = &self.0[usize::from(instruction)];
        let at = generation.checked_sub(1)?;
        let requester = kind.requester.get(at)?.get();
        (requester != ABSENT).then(|| (requester, kind.ticks[at].get()))
    }

    /// The requester reports once after its whole fence is ready. An unclassified
    /// target observation (e.g. cancellation) must never masquerade as direct.
    pub(super) fn note_ready(&self, ticket: FenceTicket, requester: u8, deferred: bool) {
        let kind = &self.0[usize::from(ticket.instruction)];
        if kind.window.get() == Window::Wrapped || !(1..=LIMIT as u64).contains(&ticket.generation)
        {
            return;
        }
        let at = ticket.generation as usize - 1;
        let bit = 1 << at;
        let recorded = kind.requester[at].get();
        if recorded != ABSENT && recorded == requester && kind.classified.get() & bit == 0 {
            if deferred {
                kind.deferred.set(kind.deferred.get() | bit);
            }
            kind.classified.set(kind.classified.get() | bit);
        }
    }

    pub(super) fn deferred(&self, instruction: bool, generation: usize) -> Option<bool> {
        if !(1..=LIMIT).contains(&generation) {
            return None;
        }
        let kind = &self.0[usize::from(instruction)];
        let bit = 1 << (generation - 1);
        (kind.classified.get() & bit != 0).then(|| kind.deferred.get() & bit != 0)
    }
}

#[cfg(test)]
mod tests;
