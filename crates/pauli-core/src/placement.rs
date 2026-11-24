//! Owner-local vCPU placement and round-robin selection, independent of ISA word size.
//! Ownership/Lock Ordering: one publisher; dispatch filters a private snapshot.
//! Invariants: a VM's lanes stay in one word; bits index slots, never grant guest entry.
use crate::arch::MAX_VCPUS;
use core::sync::atomic::{AtomicUsize, Ordering};

const BITS: usize = usize::BITS as usize;
// Align each VM's lanes so a whole stopped VM can be removed in one operation.
pub(crate) const STRIDE: usize = MAX_VCPUS.next_power_of_two();
/// Four domains fit the initial boot set and one 64-bit owner-placement word.
/// Physical directory pages pack multiple such groups; neither is a VM limit.
pub(crate) const GROUP: usize = 4;
pub(crate) const LANES: usize = GROUP * STRIDE;
pub(crate) type Placement = SlotSet<{ LANES.div_ceil(BITS) }>;
pub(crate) type Published = PublishedSet<{ LANES.div_ceil(BITS) }>;
const _: () = assert!(GROUP <= BITS && STRIDE <= BITS && BITS.is_multiple_of(STRIDE));

/// Management changes one VM's lanes at a time. A snapshot is only a hint:
/// a dispatcher pins the current slot and rechecks its owner before entry.
pub(crate) struct PublishedSet<const WORDS: usize, const WIDTH: usize = STRIDE> {
    words: [AtomicUsize; WORDS],
    end: AtomicUsize,
}
impl<const WORDS: usize, const WIDTH: usize> PublishedSet<WORDS, WIDTH> {
    pub(crate) const fn empty() -> Self {
        Self {
            words: [const { AtomicUsize::new(0) }; WORDS],
            end: AtomicUsize::new(0),
        }
    }
    pub(crate) fn snapshot(&self) -> SlotSet<WORDS, WIDTH> {
        let mut out = SlotSet::EMPTY;
        // A one-VM host need not load a full table of atomics on each dispatch.
        let end = if WORDS == 1 {
            1
        } else {
            self.end.load(Ordering::Acquire)
        };
        for (dst, src) in out.0.iter_mut().zip(&self.words[..end]) {
            *dst = src.load(Ordering::Acquire);
        }
        out
    }
    pub(crate) fn set_vm(&self, vm: usize, cpus: usize) {
        let first = vm * WIDTH;
        let index = first / BITS;
        let shift = first % BITS;
        let lanes = usize::MAX >> (BITS - WIDTH);
        let word = &self.words[index];
        // A single management writer updates lanes; readers only take snapshots.
        let value = (word.load(Ordering::Relaxed) & !(lanes << shift)) | (cpus << shift);
        word.store(value, Ordering::Release);
        if WORDS > 1 {
            let end = self.end.load(Ordering::Relaxed);
            if value != 0 && index >= end {
                self.end.store(index + 1, Ordering::Release);
            } else if value == 0 && index + 1 == end {
                let used = self.words[..index]
                    .iter()
                    .rposition(|w| w.load(Ordering::Relaxed) != 0);
                self.end.store(used.map_or(0, |i| i + 1), Ordering::Release);
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SlotSet<const WORDS: usize, const WIDTH: usize = STRIDE>([usize; WORDS]);

impl<const WORDS: usize, const WIDTH: usize> SlotSet<WORDS, WIDTH> {
    const LANES: usize = usize::MAX >> (BITS - WIDTH);
    pub(crate) const EMPTY: Self = Self([0; WORDS]);

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.0.iter().all(|&word| word == 0)
    }

    pub(crate) fn count(&self) -> usize {
        self.0.iter().map(|word| word.count_ones() as usize).sum()
    }

    #[cfg(test)]
    pub(crate) fn insert(&mut self, vm: usize, cpu: u8) {
        let flat = vm * WIDTH + usize::from(cpu);
        self.0[flat / BITS] |= 1 << (flat % BITS);
    }

    pub(crate) fn remove_vm(&mut self, vm: usize) {
        let first = vm * WIDTH;
        self.0[first / BITS] &= !(Self::LANES << (first % BITS));
    }

    /// Split a circular first group around its cursor: run its lower lanes
    /// only AFTER all later groups have had their round-robin turn.
    pub(crate) fn take_before(&mut self, cursor: usize) -> Self {
        let mut before = Self::EMPTY;
        for (i, word) in self.0.iter_mut().enumerate() {
            let count = cursor.saturating_sub(i * BITS).min(BITS);
            let upper = usize::MAX.checked_shl(count as u32).unwrap_or(0);
            before.0[i] = *word & !upper;
            *word &= upper;
        }
        before
    }

    #[cfg(test)]
    pub(crate) fn local_cpus(&self, vm: usize) -> usize {
        let first = vm * WIDTH;
        (self.0[first / BITS] >> (first % BITS)) & Self::LANES
    }

    /// Consume all lanes of the next populated VM; never scan absent VM IDs.
    pub(crate) fn pop_vm(&mut self) -> Option<(usize, usize)> {
        let (index, word) = self
            .0
            .iter_mut()
            .enumerate()
            .find(|(_, word)| **word != 0)?;
        let first = (word.trailing_zeros() as usize) & !(WIDTH - 1);
        let lanes = (*word >> first) & Self::LANES;
        *word &= !(Self::LANES << first);
        Some(((index * BITS + first) / WIDTH, lanes))
    }

    /// Consume one candidate. The common case stays in the cursor word; search
    /// later words and wrap only if its upper bits are empty. A rejected slot
    /// cannot be retried in this round, even if its lifecycle changes.
    pub(crate) fn pop_from(&mut self, cursor: usize) -> Option<usize> {
        let first = if WORDS == 1 { 0 } else { cursor / BITS };
        let local = self.0[first];
        let upper = local & (usize::MAX << (cursor % BITS));
        let (word, bits) = if upper != 0 {
            (first, upper)
        } else {
            let next = (first + 1..WORDS).chain(0..first).find(|&i| self.0[i] != 0);
            next.map_or((first, local), |i| (i, self.0[i]))
        };
        if bits == 0 {
            return None;
        }
        let bit = bits.trailing_zeros() as usize;
        self.0[word] &= !(1usize << bit);
        Some(word * BITS + bit)
    }
}
