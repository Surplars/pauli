//! One dedicated physical owner's SBI TIME endpoint, shared by every VM/vCPU
//! it dispatches. Native scheduling harts must keep using uncached timer writes.
//! Ownership/Lock Ordering: one pinned IRQ-masked physical owner, no locks.
//! Invariants: no out-of-band timer writes until this owner is disarmed.
use super::{PhysicalTimerStats, cache::Cache};
#[cfg(target_arch = "riscv64")]
use crate::host::sbi::SbiRet;

#[must_use = "retain on its physical owner and disarm before ownership handoff"]
pub struct PhysicalTimer {
    cache: Cache,
}
impl PhysicalTimer {
    pub const fn new() -> Self {
        Self {
            cache: Cache::new(),
        }
    }
    pub const fn statistics(&self) -> PhysicalTimerStats {
        self.cache.statistics()
    }
    /// # Safety
    /// Dedicated HS physical owner, no migration, SIE masked. This object is
    /// the SOLE writer of this hart's SBI TIME endpoint until disarm/handoff;
    /// native IRQ handlers or direct timer writes must not bypass its cache.
    /// A pending old compare is permitted: guest entry takes the interrupt,
    /// and idle selection replaces it with the earliest waiting deadline.
    #[cfg(target_arch = "riscv64")]
    pub unsafe fn program(&mut self, deadline: u64) -> Result<(), SbiRet> {
        self.cache.program(deadline, |deadline| {
            let result = crate::host::timer::set_sbi_timer(deadline);
            if result.error == 0 {
                Ok(())
            } else {
                Err(result)
            }
        })
    }
    /// Physical endpoint seam: the cache commits only a successful firmware
    /// call. Tests supply a private endpoint, never a fake module tree.
    pub fn program_with<E>(
        &mut self,
        deadline: u64,
        endpoint: impl FnOnce(u64) -> Result<(), E>,
    ) -> Result<(), E> {
        self.cache.program(deadline, endpoint)
    }
    /// Consume one owner lifetime using a supplied firmware endpoint.
    pub fn disarm_with<E>(
        mut self,
        endpoint: impl FnOnce(u64) -> Result<(), E>,
    ) -> Result<PhysicalTimerStats, E> {
        self.program_with(u64::MAX, endpoint)?;
        Ok(self.statistics())
    }
    /// # Safety
    /// Same exclusive-owner contract as program. Consumes the cached lifetime:
    /// after firmware stop/handoff a new owner must begin with unknown state,
    /// not reuse knowledge from before another writer touched TIME.
    #[cfg(target_arch = "riscv64")]
    pub unsafe fn disarm(mut self) -> Result<PhysicalTimerStats, SbiRet> {
        // SAFETY: the caller exclusively owns this CPU's TIME endpoint and disarms it before handoff.
        unsafe { self.program(u64::MAX) }?;
        Ok(self.statistics())
    }
}
