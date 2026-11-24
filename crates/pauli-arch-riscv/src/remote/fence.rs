//! Pure atomic fence generations and completion notifications. No CSR or
//! firmware calls: the physical owner supplies the actual local fence.
//! Ownership/Lock Ordering: atomic producers; one owner supplies actual fences before ACK.
//! Invariants: instruction and translation tickets retain independent obligations.
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Independent obligations in one sampled batch. FENCE.I does not invalidate
/// translations; neither kind can stand in for the other when requests merge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FenceBatch {
    pub translation: bool,
    pub instruction: bool,
}
impl FenceBatch {
    pub const fn single(instruction: bool) -> Self {
        Self {
            translation: !instruction,
            instruction,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FenceTicket {
    pub(super) generation: u64,
    pub(super) instruction: bool,
}
pub struct FenceQueue {
    requested: [AtomicU64; 2],
    done: [AtomicU64; 2],
    // Native-word RMW; the base RISC-V A extension has no byte AMO.
    waiters: AtomicUsize,
}
impl FenceQueue {
    pub const fn new() -> Self {
        Self {
            requested: [const { AtomicU64::new(0) }; 2],
            done: [const { AtomicU64::new(0) }; 2],
            waiters: AtomicUsize::new(0),
        }
    }
    pub fn request(&self, instruction: bool) -> FenceTicket {
        let generation = self.requested[usize::from(instruction)]
            .fetch_add(1, Ordering::Release)
            .wrapping_add(1);
        FenceTicket {
            generation,
            instruction,
        }
    }
    /// Arm AFTER publishing the generation, BEFORE kicking the target. The
    /// caller checks completed() once more before parking. One bit represents
    /// one in-VM requester; repeated requests coalesce without losing tickets.
    pub fn arm(&self, requester: u8) {
        self.waiters.fetch_or(1 << requester, Ordering::Release);
    }
    pub fn disarm(&self, requester: u8) {
        self.waiters.fetch_and(!(1 << requester), Ordering::AcqRel);
    }
    pub fn completed(&self, ticket: FenceTicket) -> bool {
        // Fewer than 2^63 outstanding generations, including counter wrap.
        self.done[usize::from(ticket.instruction)]
            .load(Ordering::Acquire)
            .wrapping_sub(ticket.generation)
            < (1 << 63)
    }
    /// Read-only empty fast path. Notifications count as work even when a
    /// fence completed before its requester armed the wake registration.
    /// A racing publication remains pending and its sender issues a kick.
    pub fn pending(&self) -> bool {
        self.waiters.load(Ordering::Acquire) != 0
            || self.requested[0].load(Ordering::Acquire) != self.done[0].load(Ordering::Relaxed)
            || self.requested[1].load(Ordering::Acquire) != self.done[1].load(Ordering::Relaxed)
    }
    /// Single physical consumer. The callback must complete the actual local
    /// fences named by BOTH batch flags before returning. Return
    /// bits name requesters to notify, not architectural guest interrupts.
    pub fn drain(&self, fence: impl FnOnce(FenceBatch)) -> usize {
        self.drain_with(fence, |_, _, _| {})
    }

    /// The target mailbox may observe the exact covered generations after
    /// the callback returns, but before publishing either completion.
    pub(super) fn drain_with(
        &self,
        fence: impl FnOnce(FenceBatch),
        covered: impl FnOnce([u64; 2], [u64; 2], usize),
    ) -> usize {
        // Consume registrations BEFORE sampling generations. A registration
        // captured here orders its earlier request before these Acquire loads;
        // a later one remains armed for another drain, even if already fenced.
        // Empty polling must not acquire exclusive cache-line ownership with
        // an AMO. A registration racing the zero read stays armed; its sender
        // kicks after arming and rechecks completion before parking.
        let notify = if self.waiters.load(Ordering::Acquire) == 0 {
            0
        } else {
            self.waiters.swap(0, Ordering::AcqRel)
        };
        let translation = self.requested[0].load(Ordering::Acquire);
        let instruction = self.requested[1].load(Ordering::Acquire);
        let prior = [
            self.done[0].load(Ordering::Relaxed),
            self.done[1].load(Ordering::Relaxed),
        ];
        let need_t = translation != prior[0];
        let need_i = instruction != prior[1];
        if need_t || need_i {
            fence(FenceBatch {
                translation: need_t,
                instruction: need_i,
            });
            covered(prior, [translation, instruction], notify);
            // Only the snapshot is covered. A request published during the
            // callback must not receive credit for this earlier fence.
            self.done[0].store(translation, Ordering::Release);
            self.done[1].store(instruction, Ordering::Release);
        }
        notify
    }
}
