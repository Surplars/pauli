//! Virtual PLIC protocol: 31 sources, bounded supervisor contexts, no physical
//! MMIO or board base addresses. One VM device owner synchronizes this state.
//! Gateways latch pending until claim; only completion re-arms a held level.
//! Ownership/Lock Ordering: one VM device owner; no native controller access.
//! Invariants: claim ownership is per context; only complete rearms a held gateway.
use crate::interrupt::VirtualInterruptController;
const CONTEXTS: usize = crate::MAX_VCPUS;

const PRIORITY_LAST: usize = 0x7c;
const PENDING: usize = 0x1000;
const ENABLE_BASE: usize = 0x2000;
const ENABLE_STRIDE: usize = 0x80;
const CONTEXT_BASE: usize = 0x200000;
const CONTEXT_STRIDE: usize = 0x1000;
pub struct VirtualPlic {
    priorities: [u8; 32],
    enabled: [u32; CONTEXTS],
    threshold: [u8; CONTEXTS],
    contexts: usize,
    levels: u32,
    pending: u32,
    busy: u32,
    in_service: [u32; CONTEXTS],
    pub claims: u64,
    pub completions: u64,
}
impl VirtualPlic {
    pub const fn new() -> Self {
        Self::with_contexts(1)
    }
    pub const fn with_contexts(contexts: usize) -> Self {
        assert!(contexts > 0 && contexts <= CONTEXTS);
        Self {
            priorities: [0; 32],
            enabled: [0; CONTEXTS],
            threshold: [0; CONTEXTS],
            contexts,
            levels: 0,
            pending: 0,
            busy: 0,
            in_service: [0; CONTEXTS],
            claims: 0,
            completions: 0,
        }
    }
    fn claim(&mut self, context: usize) -> u32 {
        // Claim ignores the notification threshold; equal priority favors the lowest ID.
        let mut eligible = self.pending & self.enabled[context];
        let mut best = 0;
        let mut priority = 0;
        while eligible != 0 {
            let source = eligible.trailing_zeros() as usize;
            eligible &= eligible - 1;
            if self.priorities[source] > priority {
                best = source as u32;
                priority = self.priorities[source];
            }
        }
        if best != 0 {
            self.pending &= !(1 << best);
            self.in_service[context] |= 1 << best;
            self.claims += 1;
        }
        best
    }
    fn complete(&mut self, context: usize, source: u32) {
        if !(1..32).contains(&source) {
            return;
        }
        let bit = 1 << source;
        if self.in_service[context] & self.enabled[context] & bit == 0 {
            return;
        }
        self.in_service[context] &= !bit;
        self.busy &= !bit;
        self.completions += 1;
        if self.levels & bit != 0 {
            self.pending |= bit;
            self.busy |= bit;
        }
    }
    pub fn read_word(&mut self, offset: usize) -> Option<u32> {
        if offset & 3 != 0 {
            return None;
        }
        Some(match offset {
            0..=PRIORITY_LAST => self.priorities[offset / 4] as u32,
            PENDING => self.pending,
            offset
                if (ENABLE_BASE..ENABLE_BASE + self.contexts * ENABLE_STRIDE).contains(&offset)
                    && offset % ENABLE_STRIDE == 0 =>
            {
                self.enabled[(offset - ENABLE_BASE) / ENABLE_STRIDE]
            }
            _ => {
                let (context, register) = self.context_register(offset)?;
                match register {
                    0 => self.threshold[context] as u32,
                    4 => self.claim(context),
                    _ => return None,
                }
            }
        })
    }
    pub fn write_word(&mut self, offset: usize, value: u32) -> Option<()> {
        if offset & 3 != 0 {
            return None;
        }
        match offset {
            0 => {} // source zero is permanently disabled
            4..=PRIORITY_LAST => self.priorities[offset / 4] = (value & 7) as u8,
            PENDING => {} // read-only pending bits
            offset
                if (ENABLE_BASE..ENABLE_BASE + self.contexts * ENABLE_STRIDE).contains(&offset)
                    && offset % ENABLE_STRIDE == 0 =>
            {
                self.enabled[(offset - ENABLE_BASE) / ENABLE_STRIDE] = value & !1
            }
            _ => {
                let (context, register) = self.context_register(offset)?;
                match register {
                    0 => self.threshold[context] = (value & 7) as u8,
                    4 => self.complete(context, value),
                    _ => return None,
                }
            }
        }
        Some(())
    }
    fn context_register(&self, offset: usize) -> Option<(usize, usize)> {
        let offset = offset.checked_sub(CONTEXT_BASE)?;
        let context = offset / CONTEXT_STRIDE;
        (context < self.contexts).then_some((context, offset % CONTEXT_STRIDE))
    }
    pub fn external_pending_for(&self, context: usize) -> bool {
        // Sampling VSEIP needs any eligible request, not the winning claim.
        let mut eligible = self.pending & self.enabled[context];
        while eligible != 0 {
            let source = eligible.trailing_zeros() as usize;
            if self.priorities[source] > self.threshold[context] {
                return true;
            }
            eligible &= eligible - 1;
        }
        false
    }
    /// Eligibility of one gateway source, independent of other source levels.
    pub fn source_pending_for(&self, context: usize, source: u32) -> bool {
        self.pending & self.enabled[context] & (1 << source) != 0
            && self.priorities[source as usize] > self.threshold[context]
    }
    /// Host lifecycle only, after every vCPU has joined. Not a guest ACK or
    /// level deassertion: discard a removed device's gateway/claim state while
    /// preserving all other sources and aggregate diagnostics.
    pub fn retire_source(&mut self, source: u32) {
        assert!((1..32).contains(&source));
        let bit = 1 << source;
        self.levels &= !bit;
        self.pending &= !bit;
        self.busy &= !bit;
        self.priorities[source as usize] = 0;
        for context in 0..self.contexts {
            self.enabled[context] &= !bit;
            self.in_service[context] &= !bit;
        }
    }
}
impl VirtualInterruptController for VirtualPlic {
    fn set_level(&mut self, source: u32, asserted: bool) {
        if !(1..32).contains(&source) {
            return;
        }
        let bit = 1 << source;
        if asserted {
            self.levels |= bit;
            if self.busy & bit == 0 {
                self.pending |= bit;
                self.busy |= bit;
            }
        } else {
            self.levels &= !bit;
        }
    }
    fn external_pending(&self) -> bool {
        self.external_pending_for(0)
    }
}

/// Protocol regression independent of any physical platform/controller.
#[cfg(any(test, feature = "selftest"))]
pub fn check_model() {
    let mut a = VirtualPlic::new();
    let b = VirtualPlic::new();
    a.set_level(10, true);
    a.set_level(11, true);
    assert!(!a.external_pending());
    a.write_word(40, 3);
    a.write_word(44, 3);
    a.write_word(0x2000, (1 << 10) | (1 << 11));
    assert!(a.external_pending());
    assert!(!b.external_pending());
    a.write_word(0x200000, 3);
    assert!(!a.external_pending());
    assert_eq!(a.read_word(0x200004), Some(10)); // claim ignores threshold
    assert_eq!(a.read_word(0x200004), Some(11));
    assert_eq!(a.read_word(0x200004), Some(0));
    a.set_level(10, false);
    a.write_word(0x200004, 10);
    a.write_word(0x200004, 10);
    a.write_word(0x200004, 99);
    assert_eq!(a.completions, 1); // duplicate/unclaimed/out-of-range ignored
    a.write_word(0x200004, 11); // held level re-pends only after completion
    a.write_word(0x200000, 0);
    assert!(a.external_pending());
    a.set_level(11, false); // a gateway request cannot be retracted
    assert!(a.external_pending());
    assert_eq!(a.read_word(0x200004), Some(11));
    a.write_word(0x200004, 11);
    assert!(!a.external_pending());
    a.set_level(0, true);
    a.set_level(32, true);
    assert_eq!(a.read_word(0), Some(0));
    assert_eq!(a.read_word(1), None);
    assert_eq!(a.write_word(0x200004, 0), Some(()));
    let mut dual = VirtualPlic::with_contexts(2);
    dual.write_word(40, 3);
    dual.write_word(0x2000, 1 << 10);
    dual.write_word(0x2080, 1 << 10);
    dual.set_level(10, true);
    assert!(dual.external_pending_for(0) && dual.external_pending_for(1));
    assert_eq!(dual.read_word(0x201004), Some(10));
    assert_eq!(dual.read_word(0x200004), Some(0));
    dual.write_word(0x200004, 10); // A peer cannot complete this context's claim.
    assert_eq!(dual.completions, 0);
    dual.set_level(10, false);
    dual.write_word(0x201004, 10);
    assert_eq!(dual.completions, 1);
    assert!(!dual.external_pending_for(0) && !dual.external_pending_for(1));
    assert_eq!(dual.read_word(0x202000), None);
    // Host removal must discard an in-service NIC without clearing an
    // unrelated pending UART or changing the diagnostic claim counters.
    dual.write_word(8, 4);
    dual.write_word(0x2000, (1 << 2) | (1 << 10));
    dual.write_word(0x2080, (1 << 2) | (1 << 10));
    dual.set_level(2, true);
    assert_eq!(dual.read_word(0x201004), Some(2));
    dual.set_level(10, true);
    let claims = dual.claims;
    dual.retire_source(2);
    assert_eq!(dual.claims, claims);
    assert_eq!(dual.completions, 1);
    assert_eq!(dual.read_word(0x1000), Some(1 << 10));
    dual.write_word(0x201004, 2); // A late completion cannot resurrect removal.
    assert_eq!(dual.completions, 1);
    assert_eq!(dual.read_word(0x200004), Some(10));
    assert_eq!(dual.read_word(0x2080), Some(1 << 10));
}
