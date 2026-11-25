//! Software RV64 IMSIC file. The VM's device owner serializes MSI and CSR
//! operations; no physical file, CSR, allocation or host address is involved.
//! Ownership/Lock Ordering: one VM file-bank owner; no physical CSR access.
//! Invariants: ID zero and unsupported IDs never escape fixed storage.
use super::registers::{EIDELIVERY, EIE, EIE_LAST, EIP, EIP_LAST, EITHRESHOLD, SELECTOR_WORD_MASK};
use pauli_config::ImsicIds;
pub const MAX_ID: usize = ImsicIds::Ids255.max_id();
pub const WORDS: usize = (MAX_ID + 1) / 64;

pub(super) const fn word_count(ids: ImsicIds) -> usize {
    ids.max_id() / 64 + 1
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct File {
    pub(super) pending: [u64; WORDS],
    pub(super) enabled: [u64; WORDS],
    ids: ImsicIds,
    pub(super) delivery: bool,
    pub(super) threshold: usize,
}
impl File {
    pub const fn new() -> Self {
        Self::with_ids(ImsicIds::Ids255)
    }
    pub const fn with_ids(ids: ImsicIds) -> Self {
        Self {
            ids,
            pending: [0; WORDS],
            enabled: [0; WORDS],
            delivery: false,
            threshold: 0,
        }
    }
    pub const fn ids(&self) -> ImsicIds {
        self.ids
    }
    pub fn inject(&mut self, id: usize) {
        if (1..=self.ids.max_id()).contains(&id) {
            self.pending[id / 64] |= 1 << (id % 64);
        }
    }
    /// A bound file in RAM is only an inbox. Eligibility is owned by hardware
    /// until put_file restores its enable/delivery state.
    pub fn has_pending(&self) -> bool {
        self.pending.iter().any(|bits| *bits != 0)
    }
    pub fn take_pending(&mut self) -> [u64; WORDS] {
        core::mem::replace(&mut self.pending, [0; WORDS])
    }
    pub fn merge_pending(&mut self, mut pending: [u64; WORDS]) {
        pending[0] &= !1;
        for (word, incoming) in self.pending[..word_count(self.ids)].iter_mut().zip(pending) {
            *word |= incoming;
        }
    }
    pub fn top(&self) -> usize {
        for index in 0..word_count(self.ids) {
            let bits = self.pending[index] & self.enabled[index];
            if bits != 0 {
                let id = index * 64 + bits.trailing_zeros() as usize;
                return if self.threshold == 0 || id < self.threshold {
                    id
                } else {
                    0
                };
            }
        }
        0
    }
    pub fn line(&self) -> bool {
        self.delivery && self.top() != 0
    }
    pub fn eligible(&self, id: usize) -> bool {
        (1..=self.ids.max_id()).contains(&id)
            && self.delivery
            && (self.threshold == 0 || id < self.threshold)
            && self.pending[id / 64] & self.enabled[id / 64] & (1 << (id % 64)) != 0
    }
    /// A read never claims; any CSR write to TOPEI claims the *old* top,
    /// independent of its written value and of EIDELIVERY.
    pub fn topei(&mut self, write: bool) -> usize {
        let id = self.top();
        if write && id != 0 {
            self.pending[id / 64] &= !(1 << (id % 64));
        }
        (id << 16) | id
    }
    pub fn indirect(
        &mut self,
        select: usize,
        clear: usize,
        set: usize,
        write: bool,
    ) -> Option<usize> {
        let old = match select {
            EIDELIVERY => usize::from(self.delivery),
            EITHRESHOLD => self.threshold,
            // All even EIP/EIE registers exist on RV64. Unsupported IDs are
            // read-only zero, not an excuse to index outside the fixed file.
            EIP..=EIP_LAST | EIE..=EIE_LAST if select & 1 == 0 => {
                let index = (select & SELECTOR_WORD_MASK) / 2;
                if index < word_count(self.ids) {
                    (if select < EIE {
                        self.pending[index]
                    } else {
                        self.enabled[index]
                    }) as usize
                } else {
                    0
                }
            }
            _ => return None,
        };
        if write {
            let value = (old & !clear) | set;
            match select {
                EIDELIVERY => self.delivery = value == 1, // WARL: only 0/1, no bypass.
                EITHRESHOLD => self.threshold = value & self.ids.max_id(),
                _ => {
                    let index = (select & SELECTOR_WORD_MASK) / 2;
                    if index < word_count(self.ids) {
                        let value = value as u64 & if index == 0 { !1 } else { u64::MAX };
                        if select < EIE {
                            self.pending[index] = value;
                        } else {
                            self.enabled[index] = value;
                        }
                    }
                }
            }
        }
        Some(old)
    }
}
