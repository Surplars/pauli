//! Minimal inactive HS identity map; no RTOS allocator or user address space.
//! Ownership/Lock Ordering: boot owner fills the retained root before publication.
//! Invariants: 1 GiB leaves, U=0; activation requires all live HS ranges mapped.
use pauli_hal::addr::{MemPerms, MmError, PhysAddr};
const GIGAPAGE: usize = 1 << 30;
const LIMIT: usize = 1 << 38;
const PTE_VALID: u64 = 1;
const PTE_READ: u64 = 2;
const PTE_WRITE: u64 = 4;
const PTE_EXECUTE: u64 = 8;
const PTE_ACCESSED_DIRTY: u64 = (1 << 6) | (1 << 7);
const SATP_SV39: usize = 8 << 60;

#[repr(C, align(4096))]
pub struct IdentityRoot {
    entries: [u64; 512],
}
impl IdentityRoot {
    pub const fn new() -> Self {
        Self { entries: [0; 512] }
    }
    /// Deliberately coarse bootstrap mappings. Fine-grained host W^X is a
    /// separate mapping policy; this does not change guest G-stage authority.
    pub fn map(&mut self, base: PhysAddr, perms: MemPerms) -> Result<(), MmError> {
        if !base.is_aligned(GIGAPAGE) {
            return Err(MmError::InvalidAlignment);
        }
        if base.raw() >= LIMIT {
            return Err(MmError::InvalidRegion);
        }
        if perms.is_empty() || (perms.contains(MemPerms::WRITE) && !perms.contains(MemPerms::READ))
        {
            return Err(MmError::InvalidPermissions);
        }
        let slot = &mut self.entries[base.raw() / GIGAPAGE];
        if *slot != 0 {
            return Err(MmError::AlreadyMapped);
        }
        let mut bits = PTE_VALID | PTE_ACCESSED_DIRTY;
        if perms.contains(MemPerms::READ) {
            bits |= PTE_READ;
        }
        if perms.contains(MemPerms::WRITE) {
            bits |= PTE_WRITE;
        }
        if perms.contains(MemPerms::EXECUTE) {
            bits |= PTE_EXECUTE;
        }
        *slot = ((base.raw() as u64 >> 12) << 10) | bits;
        Ok(())
    }
    /// # Safety
    /// This retained root is identity-addressable, immutable after activation,
    /// and maps every active code, stack, table, RAM and device range used by HS.
    /// Sv39 exists; IRQs are masked. Other CPUs activate only after publication.
    pub unsafe fn activate(&'static self) {
        let satp = SATP_SV39 | (self as *const Self as usize >> 12);
        // SAFETY: the caller retains a complete identity map across the SATP change.
        unsafe {
            core::arch::asm!("fence rw, rw", "csrw satp, {satp}", "sfence.vma zero, zero", satp = in(reg) satp, options(nostack));
        }
    }
}
