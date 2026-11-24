//! Admission for runtime children sponsored by a management VM.
//! Ownership/Lock Ordering: the sole manager sets limits and changes the live set.
//! Invariants: stopped children stay charged; a sponsor cannot retire before its children.
use super::{HvArch, HvError, Result, VmManager};
use pauli_config::VmConfig;
use pauli_control::VmId;

/// Allocated topology and exact guest RAM, not runnable time or host metadata.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Budget {
    guests: u32,
    vcpus: u64,
    memory_bytes: u64,
}
impl core::fmt::Display for Budget {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (size, unit) = if self.memory_bytes.is_multiple_of(1 << 20) {
            (self.memory_bytes >> 20, "MiB")
        } else {
            (self.memory_bytes >> 10, "KiB") // Every allocation is page-aligned.
        };
        write!(
            f,
            "guests={} vcpus={} memory={size}{unit}",
            self.guests, self.vcpus
        )
    }
}
impl Budget {
    pub(crate) fn from_mib(guests: u32, vcpus: u32, memory_mib: u32) -> Self {
        Self {
            guests,
            vcpus: vcpus.into(),
            memory_bytes: u64::from(memory_mib) << 20,
        }
    }
    fn charge(&mut self, config: &VmConfig) {
        // One ID belongs to the sponsor. Each other u32 ID charges at most
        // MAX_RAM_BYTES (1 GiB) and MAX_GUEST_VCPUS, so both totals fit in u64.
        self.guests += 1;
        self.vcpus += u64::from(config.vcpus);
        self.memory_bytes += config.memory_size;
    }
    fn fits(self, limit: Self) -> bool {
        self.guests <= limit.guests
            && self.vcpus <= limit.vcpus
            && self.memory_bytes <= limit.memory_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_above_the_largest_cli_limit_does_not_wrap() {
        let limit = Budget::from_mib(u32::MAX, u32::MAX, u32::MAX);
        let child = VmConfig::new("linux").unwrap();
        let mut used = Budget {
            guests: u32::MAX / 4,
            ..limit
        };
        used.charge(&child);
        assert!(used.vcpus > limit.vcpus && used.memory_bytes > limit.memory_bytes);
        assert!(!used.fits(limit));
    }
}
impl<A: HvArch> VmManager<A> {
    /// Derive the charge from published children. No rollback ledger can drift
    /// when a build fails, a CPU is pinned, or another authorized caller destroys.
    fn usage(&self, sponsor: VmId) -> Budget {
        let mut used = Budget::default();
        for vm in self.vms().filter(|vm| vm.sponsor == Some(sponsor)) {
            used.charge(&vm.config);
        }
        used
    }
    pub(crate) fn budget(&self, slot: usize) -> Result<(Budget, Option<Budget>)> {
        let limit = *self.vm(slot)?.quota.lock();
        Ok((self.usage(VmId(slot as u32 + 1)), limit))
    }
    pub(crate) fn set_budget(&self, slot: usize, limit: Option<Budget>) -> Result<()> {
        let vm = self.vm(slot)?;
        if limit.is_some_and(|cap| !self.usage(VmId(slot as u32 + 1)).fits(cap)) {
            return Err(HvError::Busy);
        }
        *vm.quota.lock() = limit;
        Ok(())
    }
    pub(super) fn admit(&self, sponsor: Option<VmId>, config: &VmConfig) -> Result<()> {
        let Some(id) = sponsor else { return Ok(()) };
        let slot = id.0.checked_sub(1).ok_or(HvError::NotFound)? as usize;
        let limit = *self.vm(slot)?.quota.lock();
        let Some(limit) = limit else { return Ok(()) };
        let mut used = self.usage(id);
        used.charge(config);
        if used.fits(limit) {
            Ok(())
        } else {
            Err(HvError::Exhausted)
        }
    }
}
