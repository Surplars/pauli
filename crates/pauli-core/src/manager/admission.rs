//! Admission for runtime children sponsored by a management VM.
//! Ownership/Lock Ordering: the sole manager sets limits and changes the live set.
//! Invariants: stopped children stay charged; a sponsor cannot retire before its children.
use super::{HvArch, HvError, Result, VmManager};
use pauli_config::VmConfig;
use pauli_control::VmId;

/// Allocated topology, exact guest RAM and the page-rounded charge for each leased channel.
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
        } else if self.memory_bytes.is_multiple_of(1 << 10) {
            (self.memory_bytes >> 10, "KiB")
        } else {
            (self.memory_bytes, "B")
        };
        write!(
            f,
            "guests={} vcpus={} memory={size}{unit}",
            self.guests, self.vcpus
        )
    }
}
impl Budget {
    pub const fn from_bytes(guests: u32, vcpus: u64, memory_bytes: u64) -> Self {
        Self {
            guests,
            vcpus,
            memory_bytes,
        }
    }
    pub(crate) fn from_mib(guests: u32, vcpus: u32, memory_mib: u32) -> Self {
        Self::from_bytes(guests, vcpus.into(), u64::from(memory_mib) << 20)
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
    fn byte_limits_keep_full_precision() {
        const EXACT: Budget = Budget::from_bytes(8, (1 << 32) + 8, (512 << 10) + 1);
        assert_eq!(EXACT.guests, 8);
        assert_eq!(EXACT.vcpus, (1 << 32) + 8);
        assert_eq!(EXACT.memory_bytes, 524289);
        assert_eq!(
            std::format!("{EXACT}"),
            "guests=8 vcpus=4294967304 memory=524289B"
        );
        assert!(!EXACT.fits(Budget::from_bytes(8, EXACT.vcpus, EXACT.memory_bytes - 1)));
        assert_eq!(Budget::from_mib(8, 8, 1), Budget::from_bytes(8, 8, 1 << 20));
    }

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
    fn usage(&self, sponsor: VmId) -> (Budget, u8) {
        let mut used = Budget::default();
        let mut channels = 0u8;
        for vm in self.vms().filter(|vm| vm.sponsor == Some(sponsor)) {
            used.charge(&vm.config);
            if let Some(lease) = vm.config.net_lease {
                channels |= 1u8 << lease.channel;
            }
        }
        used.memory_bytes +=
            u64::from(channels.count_ones()) * super::leases::LeaseHandle::BYTES as u64;
        (used, channels)
    }
    pub(crate) fn budget(&self, slot: usize) -> Result<(Budget, Option<Budget>)> {
        let limit = *self.vm(slot)?.quota.lock();
        Ok((self.usage(VmId(slot as u32 + 1)).0, limit))
    }
    pub(crate) fn set_budget(&self, slot: usize, limit: Option<Budget>) -> Result<()> {
        let vm = self.vm(slot)?;
        if limit.is_some_and(|cap| !self.usage(VmId(slot as u32 + 1)).0.fits(cap)) {
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
        let (mut used, channels) = self.usage(id);
        used.charge(config);
        if config
            .net_lease
            .is_some_and(|lease| channels & (1u8 << lease.channel) == 0)
        {
            used.memory_bytes += super::leases::LeaseHandle::BYTES as u64;
        }
        if used.fits(limit) {
            Ok(())
        } else {
            Err(HvError::Exhausted)
        }
    }
}
