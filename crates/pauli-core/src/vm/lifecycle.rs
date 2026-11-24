//! One atomic admission/stop linearization point for every VM vCPU.
//! Ownership/Lock Ordering: atomics only; management commits join after all proofs.
//! Invariants: stop closes entry before checking active bits; fault dominates reset.
//! State: Joined --publish--> Ready --stop/fault/reset--> Stopping --join--> Joined.
use crate::{HvError, Result, arch::MAX_VCPUS};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
const READY: usize = 1;
const STOP: usize = 2;
const FAULT: usize = 4;
const RESET: usize = 8;
const JOINED: usize = 16;
const HOST_STOP: usize = 32;
const REBUILDING: usize = 64;
const OWNER_SHIFT: usize = 8;
const _: () = assert!(OWNER_SHIFT + MAX_VCPUS <= usize::BITS as usize);
const OWNERS: usize = ((1 << MAX_VCPUS) - 1) << OWNER_SHIFT;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    Ready,
    Stopping,
    Stopped,
    Faulted,
}
/// Management-only observation lives with the VM, not in a second ID-sized table.
/// Zero is unseen (epochs start at one); no store is needed on an unchanged poll.
pub(crate) struct Notice(AtomicU64);
impl Notice {
    pub(crate) const fn new() -> Self {
        Self(AtomicU64::new(0))
    }
    pub(crate) fn record(&self, epoch: u32, state: State) -> (u32, Option<State>) {
        const STATES: [State; 4] = [
            State::Ready,
            State::Stopping,
            State::Stopped,
            State::Faulted,
        ];
        let stamp = (u64::from(epoch) << 2) | state as u64;
        let old = self.0.load(Ordering::Relaxed);
        if stamp != old {
            self.0.store(stamp, Ordering::Relaxed);
        }
        (
            (old >> 2) as u32,
            (old != 0).then(|| STATES[(old & 3) as usize]),
        )
    }
}
pub struct Lifecycle(AtomicUsize);
pub struct Rebuild<'a> {
    lifecycle: &'a Lifecycle,
    old: usize,
    published: bool,
}
impl Rebuild<'_> {
    /// A stop/fault published during rebuild cancels admission of the new epoch.
    pub fn publish(mut self) -> Result<()> {
        self.lifecycle
            .0
            .compare_exchange(
                JOINED | REBUILDING,
                READY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| HvError::Busy)?;
        self.published = true;
        Ok(())
    }
}
impl Drop for Rebuild<'_> {
    fn drop(&mut self) {
        if !self.published {
            let old_fault = self.old & FAULT;
            let _ = self
                .lifecycle
                .0
                .try_update(Ordering::AcqRel, Ordering::Acquire, |word| {
                    Some((word & !REBUILDING) | STOP | JOINED | old_fault)
                });
        }
    }
}
impl Lifecycle {
    pub const fn new() -> Self {
        Self(AtomicUsize::new(STOP | JOINED))
    }
    pub fn state(&self) -> State {
        let word = self.0.load(Ordering::Acquire);
        if word & JOINED != 0 {
            if word & FAULT != 0 {
                State::Faulted
            } else {
                State::Stopped
            }
        } else if word & STOP != 0 {
            State::Stopping
        } else {
            State::Ready
        }
    }
    pub fn publish(&mut self) {
        self.0.store(READY, Ordering::Release);
    }
    pub fn begin_rebuild(&self) -> Result<Rebuild<'_>> {
        let word = self.0.load(Ordering::Acquire);
        if word & JOINED == 0 || word & REBUILDING != 0 {
            return Err(HvError::Busy);
        }
        self.0
            .compare_exchange(
                word,
                JOINED | REBUILDING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| HvError::Busy)?;
        Ok(Rebuild {
            lifecycle: self,
            old: word,
            published: false,
        })
    }
    pub fn request_stop(&self, fault: bool, reset: bool) {
        self.0.fetch_or(
            STOP | if fault { FAULT } else { 0 } | if reset { RESET } else { 0 },
            Ordering::AcqRel,
        );
    }
    pub fn stopping(&self) -> bool {
        self.0.load(Ordering::Acquire) & (STOP | JOINED) != 0
    }
    pub fn joined(&self) -> bool {
        self.0.load(Ordering::Acquire) & JOINED != 0
    }
    pub fn reset_requested(&self) -> bool {
        let word = self.0.load(Ordering::Acquire);
        word & RESET != 0 && word & (FAULT | HOST_STOP) == 0
    }
    pub fn cancel_reset(&self) {
        self.0.fetch_or(STOP | HOST_STOP, Ordering::AcqRel);
    }
    /// Owner work includes off-CPU retirement. JOINED freezes all owner access
    /// before management rewrites RAM, contexts, devices or mailbox epochs.
    pub fn acquire_owner(&self, index: u8) -> bool {
        debug_assert!((index as usize) < MAX_VCPUS);
        let bit = 1usize << (OWNER_SHIFT + index as usize);
        let mut word = self.0.load(Ordering::Acquire);
        loop {
            if word & JOINED != 0 || word & bit != 0 {
                return false;
            }
            match self.0.compare_exchange_weak(
                word,
                word | bit,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(next) => word = next,
            }
        }
    }
    pub fn release_owner(&self, index: u8) {
        self.0
            .fetch_and(!(1 << (OWNER_SHIFT + index as usize)), Ordering::Release);
    }
    pub fn can_enter(&self) -> bool {
        self.0.load(Ordering::Acquire) & (READY | STOP | JOINED) == READY
    }
    pub fn owners_idle(&self) -> bool {
        self.0.load(Ordering::Acquire) & OWNERS == 0
    }
    pub(crate) fn require_device_join(&self) {
        self.0.fetch_and(!JOINED, Ordering::AcqRel);
    }
    pub fn commit_join(&self, translations_retired: bool, devices_joined: bool) -> bool {
        if !translations_retired || !devices_joined {
            return false;
        }
        let word = self.0.load(Ordering::Acquire);
        if word & STOP == 0 || word & OWNERS != 0 {
            return false;
        }
        self.0
            .compare_exchange(word, word | JOINED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}
