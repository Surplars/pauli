//! RV64 guest software timers and dedicated-owner physical deadline reuse.
//! No Sstc or board compare register is required. Guest time is physical `time`
//! counter (htimedelta = 0), including intervals when the VM is not running.
//! Deadlines use the SBI unsigned absolute-time comparison, not wrapping
//! duration arithmetic. u64::MAX is the explicit disarm sentinel.

//! Ownership/Lock Ordering: each vCPU owns virtual state; one native CPU owns its physical cache.
//! Invariants: a host deadline never renews the slice or acknowledges guest STIP.
mod cache;

mod physical;
pub use cache::Statistics as PhysicalTimerStats;
pub use physical::PhysicalTimer;

pub const VSTIP: usize = 1 << 6;
pub const VS_STIP: usize = 1 << 5;

/// Per-vCPU state, never a per-hart/global pending bit. Expiry latches a level
/// until a new TIME request acknowledges it; entering the guest does not clear
/// it. A descheduled vCPU owns no independent physical timer and refreshes on
/// next entry; the dispatcher replaces its shared compare for dispatch/idle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VirtualTimer {
    deadline: Option<u64>,
    pending: bool,
}

impl VirtualTimer {
    pub const fn new() -> Self {
        Self {
            deadline: None,
            pending: false,
        }
    }

    pub const fn deadline(&self) -> Option<u64> {
        self.deadline
    }

    pub const fn pending(&self) -> bool {
        self.pending
    }

    /// SBI TIME must clear the old pending level when setting a future time,
    /// even if guest STIE/SIE is masked. An already expired request is pending
    /// immediately, without repeatedly arming an expired physical compare.
    pub fn set_deadline(&mut self, deadline: u64, now: u64) {
        self.deadline = (deadline != u64::MAX).then_some(deadline);
        self.pending = false;
        self.refresh(now);
    }

    pub fn refresh(&mut self, now: u64) {
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            self.pending = true;
        }
    }

    /// Once pending, the guest owns acknowledgement; the physical compare is
    /// reserved for host preemption. Repeated IRQ delivery must not repeatedly
    /// shorten/renew the host's absolute scheduling budget.
    pub fn physical_deadline(&self, host_deadline: u64) -> u64 {
        match self.deadline {
            Some(deadline) if !self.pending => deadline.min(host_deadline),
            _ => host_deadline,
        }
    }
}
