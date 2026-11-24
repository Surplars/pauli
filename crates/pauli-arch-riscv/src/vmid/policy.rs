//! Hart-local tag ownership. A binding is reusable only for the same VM epoch
//! AND immutable G-stage root. Retiring/restarting a VM never reuses its epoch.
//! Numeric tags deliberately have no cross-hart identity.
//! Ownership/Lock Ordering: one physical owner mutates its private cache.
//! Invariants: root plus epoch identify a lifetime; tag reuse fences implicit walks.
pub const TAGS: usize = 4;
pub const TAGGED: usize = 1;
pub const INVALIDATE: usize = 2;
pub const VS_INVALIDATE: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub epoch: u32,
    pub root: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    pub tag: u16,
    /// G-stage reuse/cold entry requires both fences.
    pub flush: bool,
    /// Guest ASIDs belong to virtual harts, not the physical owner. Switching
    /// siblings under one retained VMID must discard their VS-only mappings.
    pub vs_flush: bool,
}
impl Plan {
    pub const fn flags(self) -> usize {
        if self.tag == 0 {
            0
        } else {
            TAGGED
                | if self.flush {
                    INVALIDATE
                } else if self.vs_flush {
                    VS_INVALIDATE
                } else {
                    0
                }
        }
    }
}
pub struct Cache {
    bindings: [Option<Key>; TAGS],
    contexts: [u8; TAGS],
    capacity: usize,
    next: usize,
}
impl Cache {
    pub const fn new(available: usize) -> Self {
        Self {
            bindings: [None; TAGS],
            contexts: [0; TAGS],
            capacity: if available < TAGS { available } else { TAGS },
            next: 0,
        }
    }
    pub fn find(&self, key: Key) -> Option<u16> {
        if key.epoch == 0 || self.capacity == 0 {
            return Some(0);
        }
        self.bindings[..self.capacity]
            .iter()
            .position(|entry| *entry == Some(key))
            .map(|index| index as u16 + 1)
    }
    /// The caller must execute the returned invalidation BEFORE guest fetch.
    /// No callback/reentry is permitted between select and that invalidation.
    pub fn select(&mut self, key: Key, context: u8, fresh_context: bool) -> Plan {
        if let Some(tag) = self.find(key) {
            let switched = if tag == 0 {
                true
            } else {
                let previous = &mut self.contexts[usize::from(tag) - 1];
                let switched = *previous != context;
                *previous = context;
                switched
            };
            return Plan {
                tag,
                flush: tag == 0 || fresh_context,
                vs_flush: tag == 0 || fresh_context || switched,
            };
        }
        // Retirement creates holes. Refill one before evicting a live peer;
        // cold restart must not churn unrelated bindings when space is free.
        let index = self.bindings[..self.capacity]
            .iter()
            .position(Option::is_none)
            .unwrap_or(self.next);
        self.next = (index + 1) % self.capacity;
        self.bindings[index] = Some(key);
        self.contexts[index] = context;
        Plan {
            tag: index as u16 + 1,
            flush: true,
            vs_flush: true,
        }
    }
    /// Forget only AFTER GVMA+VVMA have terminated this owner's old walks.
    pub fn forget(&mut self, key: Key) {
        for entry in &mut self.bindings[..self.capacity] {
            if *entry == Some(key) {
                *entry = None;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Statistics {
    /// Numeric tag used by the last entry, not a globally stable VM identifier.
    pub vmid: u16,
    /// Entries whose identity was retained and needed no transition fences.
    pub warm_entries: u64,
    /// Entry invalidation events, not cycles or individual fence instructions.
    pub entry_flushes: u64,
    pub remote_syncs: u64,
    /// Actual local target VVMA and FENCE.I operations, not just queued ACKs.
    /// A mixed batch increments both; an evicted TLB binding needs no VVMA.
    pub remote_tlb: u64,
    pub remote_icache: u64,
    pub retirements: u64,
}
