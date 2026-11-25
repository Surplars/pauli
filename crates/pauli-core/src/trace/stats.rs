//! Little-endian RV64 statistics wire format. Halt every participating hart for a coherent dump.
use crate::arch::MAX_CPUS;
use core::{
    mem::{align_of, offset_of, size_of},
    sync::atomic::{AtomicU64, Ordering},
};

pub const HS_STATS_MAGIC: u64 = 0x5041_554c_4953_5431;
pub const HS_STATS_VERSION: u64 = 2;
pub const UNREGISTERED_HART: u64 = u64::MAX;
const HEADER_BYTES: usize = 64;
const OWNER_STRIDE: usize = 128;

/// The RV64 linker places the symbol at 64 mod 128, so the first and every
/// subsequent owner start on a 128-byte boundary. A Rust alignment of 128
/// would insert padding between the header and owner zero, changing the wire ABI.
#[repr(C, align(64))]
pub struct HsStats {
    pub magic: AtomicU64,
    pub version: AtomicU64,
    pub header_bytes: AtomicU64,
    pub owner_stride: AtomicU64,
    pub host_cpus: AtomicU64,
    pub sstc_owners: AtomicU64,
    pub exclusive_owners: AtomicU64,
    pub reserved: AtomicU64,
    pub owners: [OwnerStats; MAX_CPUS],
}

#[repr(C, align(64))]
pub struct OwnerStats {
    pub hart_id: AtomicU64,
    pub vm_exits: AtomicU64,
    pub wfi_exits: AtomicU64,
    pub sbi_timer_exits: AtomicU64,
    pub sstc_timer_exits: AtomicU64,
    pub mmio_exits: AtomicU64,
    pub other_exits: AtomicU64,
    pub vtimer_irqs: AtomicU64,
    pub ext_irqs: AtomicU64,
    pub last_scause: AtomicU64,
    pub last_sepc: AtomicU64,
    pub last_stval: AtomicU64,
    pub idle_wfi: AtomicU64,
    pub mail_sends: AtomicU64,
    pub mail_recvs: AtomicU64,
    pub reserved: AtomicU64,
}

const _: () = {
    assert!(MAX_CPUS == 64);
    assert!(size_of::<HsStats>() == HEADER_BYTES + MAX_CPUS * OWNER_STRIDE);
    assert!(size_of::<OwnerStats>() == OWNER_STRIDE);
    assert!(align_of::<HsStats>() == 64 && align_of::<OwnerStats>() == 64);
    assert!(offset_of!(HsStats, owners) == HEADER_BYTES);
};

impl OwnerStats {
    const fn new() -> Self {
        Self {
            hart_id: AtomicU64::new(UNREGISTERED_HART),
            vm_exits: AtomicU64::new(0),
            wfi_exits: AtomicU64::new(0),
            sbi_timer_exits: AtomicU64::new(0),
            sstc_timer_exits: AtomicU64::new(0),
            mmio_exits: AtomicU64::new(0),
            other_exits: AtomicU64::new(0),
            vtimer_irqs: AtomicU64::new(0),
            ext_irqs: AtomicU64::new(0),
            last_scause: AtomicU64::new(0),
            last_sepc: AtomicU64::new(0),
            last_stval: AtomicU64::new(0),
            idle_wfi: AtomicU64::new(0),
            mail_sends: AtomicU64::new(0),
            mail_recvs: AtomicU64::new(0),
            reserved: AtomicU64::new(0),
        }
    }
}
impl HsStats {
    pub const fn new() -> Self {
        Self {
            magic: AtomicU64::new(HS_STATS_MAGIC),
            version: AtomicU64::new(HS_STATS_VERSION),
            header_bytes: AtomicU64::new(HEADER_BYTES as u64),
            owner_stride: AtomicU64::new(OWNER_STRIDE as u64),
            host_cpus: AtomicU64::new(0),
            sstc_owners: AtomicU64::new(0),
            exclusive_owners: AtomicU64::new(0),
            reserved: AtomicU64::new(0),
            owners: [const { OwnerStats::new() }; MAX_CPUS],
        }
    }

    /// Called once on the boot hart, with validated topology, before launching workers.
    pub(crate) fn init_host(&self, host_cpus: usize, exclusive_owners: u64) {
        assert!((1..=MAX_CPUS).contains(&host_cpus));
        assert!(exclusive_owners & !(u64::MAX >> (MAX_CPUS - host_cpus)) == 0);
        self.exclusive_owners
            .store(exclusive_owners, Ordering::Relaxed);
        self.host_cpus.store(host_cpus as u64, Ordering::Release);
    }

    /// A pinned owner registers once after probing its hardware and before guest dispatch.
    pub(crate) fn register_owner(&self, owner: usize, hart_id: usize, sstc: bool) {
        if owner >= self.host_cpus.load(Ordering::Acquire) as usize
            || hart_id as u64 == UNREGISTERED_HART
        {
            return;
        }
        if sstc {
            self.sstc_owners.fetch_or(1u64 << owner, Ordering::Relaxed);
        }
        self.owners[owner]
            .hart_id
            .store(hart_id as u64, Ordering::Release);
    }
}

#[unsafe(no_mangle)]
#[unsafe(link_section = ".pauli_hs_stats")]
pub static PAULI_HS_STATS: HsStats = HsStats::new();

pub fn init_host(host_cpus: usize, exclusive_owners: u64) {
    PAULI_HS_STATS.init_host(host_cpus, exclusive_owners);
}
pub fn register_owner(owner_idx: usize, hart_id: usize, sstc_active: bool) {
    PAULI_HS_STATS.register_owner(owner_idx, hart_id, sstc_active);
}
