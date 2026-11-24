//! Bounded, read-only VS Sv39 instruction translation. Every untrusted PTE
//! address is a GPA and must pass the immutable G-stage R check before a host
//! load. This is validation/decoding, not an alternative hardware MMU: no A/D
//! emulation, guest data-copy API, Sv48/57, PBMT, NAPOT or big-endian tables.
//! Ownership/Lock Ordering: read-only guest snapshots, no host locks.
//! Invariants: every PTE GPA passes immutable outer-stage R authority before a volatile read.
use pauli_hal::addr::{MemPerms, MmError, PAGE_SIZE, PhysAddr};

const V: u64 = 1;
const R: u64 = 2;
const W: u64 = 4;
const X: u64 = 8;
const U: u64 = 16;
const A: u64 = 64;
const D: u64 = 128;
const PPN: u64 = (1 << 44) - 1;

/// Immutable outer-map authority used by instruction decoding. A returned
/// address remains a scalar until an unsafe fetch's retained-RAM contract holds.
pub trait ReadOnlyStage2 {
    fn translate(&self, gpa: PhysAddr) -> Option<PhysAddr>;
    fn translate_readable(&self, gpa: PhysAddr) -> Option<PhysAddr>;
    fn is_executable(&self, gpa: PhysAddr) -> bool;
    /// # Safety
    /// The mapped instruction pages satisfy the retained foreign-RAM contract.
    unsafe fn fetch_bare_u32(&self, pc: usize) -> Option<u32>;
}

/// Guest-owned page storage. The platform assigns its GPA and retains backing;
/// constructing or editing an inactive guest table does not edit G-stage.
#[repr(C, align(4096))]
pub struct VStageTable([u64; 512]);
impl VStageTable {
    pub const fn new() -> Self {
        Self([0; 512])
    }
    pub fn clear(&mut self) {
        self.0.fill(0);
    }
    pub fn set(&mut self, index: usize, pte: u64) -> Result<(), MmError> {
        *self.0.get_mut(index).ok_or(MmError::InvalidRegion)? = pte;
        Ok(())
    }
}
const _: () = assert!(core::mem::size_of::<VStageTable>() == PAGE_SIZE);

pub fn sv39_satp(root: PhysAddr) -> Option<usize> {
    (root.is_aligned(PAGE_SIZE) && root.raw() < (1usize << 41))
        .then_some((8usize << 60) | (root.raw() >> 12))
}
pub fn branch_pte(gpa: PhysAddr) -> Option<u64> {
    (gpa.is_aligned(PAGE_SIZE) && gpa.raw() < (1usize << 56))
        .then_some(((gpa.raw() as u64 >> 12) << 10) | V)
}
pub fn leaf_pte(gpa: PhysAddr, perms: MemPerms) -> Option<u64> {
    if perms.is_empty() || (perms.contains(MemPerms::WRITE) && !perms.contains(MemPerms::READ)) {
        return None;
    }
    let mut pte = branch_pte(gpa)? | A | D;
    if perms.contains(MemPerms::READ) {
        pte |= R;
    }
    if perms.contains(MemPerms::WRITE) {
        pte |= W;
    }
    if perms.contains(MemPerms::EXECUTE) {
        pte |= X;
    }
    Some(pte) // VS may request W+X; only G-stage owns the outer W^X policy.
}

/// Resolve a VS supervisor instruction address, including its final G-stage X
/// permission. At most three PTE reads, including cyclic/malformed tables.
/// Only already-accessed U=0 executable leaves are accepted; S-mode cannot
/// execute U pages even with SUM. Superpage lower PPN bits must be zero.
///
/// # Safety
/// Accessed guest table HPAs must be retained identity-mapped host-readable
/// little-endian foreign-mutable RAM, not MMIO or aliased Rust references.
/// Guest PTE writers may race these aligned volatile snapshots: every sampled
/// next address is checked again through immutable G-stage permissions. This
/// is not a linearizable walk; a racing guest may get a rejected decode/fault,
/// never authority to follow an unchecked host pointer.
pub unsafe fn instruction_gpa(
    space: &impl ReadOnlyStage2,
    satp: usize,
    va: usize,
) -> Option<PhysAddr> {
    // SAFETY: the fetch contract retains foreign RAM and every sampled address passes outer-stage permission checks.
    let gpa = unsafe { supervisor_gpa(space, satp, va, X | A) }?;
    space.is_executable(gpa).then_some(gpa)
}

/// Resolve only the inner-stage supervisor data access for MMIO validation.
/// The final GPA intentionally need not be G-stage mapped: it is a device
/// trap. All PTE reads must still be assigned readable RAM, so an implicit
/// page-table access cannot be mistaken for explicit MMIO when htinst is zero.
/// U pages, MXR-only reads, and A/D repair are deliberately unsupported here.
/// # Safety
/// The same retained foreign-RAM snapshot contract as `instruction_gpa`.
pub unsafe fn supervisor_data_gpa(
    space: &impl ReadOnlyStage2,
    satp: usize,
    va: usize,
    write: bool,
) -> Option<PhysAddr> {
    // SAFETY: the fetch contract retains foreign RAM and every sampled address passes outer-stage permission checks.
    unsafe { supervisor_gpa(space, satp, va, if write { R | W | A | D } else { R | A }) }
}

unsafe fn supervisor_gpa(
    space: &impl ReadOnlyStage2,
    satp: usize,
    va: usize,
    required: u64,
) -> Option<PhysAddr> {
    let gpa = match satp >> 60 {
        0 if satp == 0 => PhysAddr::new(va),
        8 => {
            if ((((va as i64) << 25) >> 25) as usize) != va {
                return None;
            }
            let mut table = (satp & PPN as usize) << 12;
            let mut leaf = None;
            for level in (0..3).rev() {
                let offset = ((va >> (12 + level * 9)) & 511) * 8;
                let pte_gpa = PhysAddr::new(table.checked_add(offset)?);
                let hpa = space.translate_readable(pte_gpa)?;
                // SAFETY: the fetch contract retains foreign RAM and every sampled address passes outer-stage permission checks.
                let pte = unsafe { hpa.as_ptr::<u64>().read_volatile() };
                if pte >> 54 != 0 || pte & V == 0 || (pte & W != 0 && pte & R == 0) {
                    return None;
                }
                let base = (((pte >> 10) & PPN) as usize) << 12;
                if pte & (R | X) != 0 {
                    let mask = (1usize << (12 + level * 9)) - 1;
                    if pte & (required | U) != required || base & mask != 0 {
                        return None;
                    }
                    leaf = Some(PhysAddr::new(base | (va & mask)));
                    break;
                }
                if level == 0 || pte & (U | A | D) != 0 {
                    return None;
                }
                table = base;
            }
            leaf?
        }
        _ => return None,
    };
    Some(gpa)
}

/// Fetch one aligned halfword, including the last halfword of a mapped page.
/// # Safety
/// The retained executable-RAM snapshot contract of `fetch_u32` applies.
pub unsafe fn fetch_u16(space: &impl ReadOnlyStage2, satp: usize, pc: usize) -> Option<u16> {
    if pc & 1 != 0 {
        return None;
    }
    // SAFETY: the fetch contract retains foreign RAM and every sampled address passes outer-stage permission checks.
    let gpa = unsafe { instruction_gpa(space, satp, pc) }?;
    let hpa = space.translate(gpa)?;
    // SAFETY: the fetch contract retains foreign RAM and every sampled address passes outer-stage permission checks.
    Some(unsafe { hpa.as_ptr::<u16>().read_volatile() })
}

/// Length-aware instruction snapshot for MMIO decode. A page-contained 32-bit
/// instruction needs ONE VS walk, not a halfword fetch followed by two more
/// walks of the same instruction. The immutable G-stage maps whole pages, so
/// the checked first half also authorizes its same-page upper half. A boundary
/// crossing still resolves/checks the second VS/G-stage page independently.
/// # Safety
/// The retained, foreign executable-RAM contract of `fetch_u32` applies. A
/// racing VS PTE update cannot revoke the retained HPA's outer-stage ownership;
/// this call reuses only its local snapshot, never caches it across exits.
pub unsafe fn fetch_instruction(
    space: &impl ReadOnlyStage2,
    satp: usize,
    pc: usize,
) -> Option<(u32, usize)> {
    if pc & 1 != 0 {
        return None;
    }
    // SAFETY: the fetch contract retains foreign RAM and every sampled address passes outer-stage permission checks.
    let gpa = unsafe { instruction_gpa(space, satp, pc) }?;
    let hpa = space.translate(gpa)?;
    // SAFETY: the fetch contract retains foreign RAM and every sampled address passes outer-stage permission checks.
    let low = unsafe { hpa.as_ptr::<u16>().read_volatile() };
    if low & 3 != 3 {
        return Some((u32::from(low), 2));
    }
    let high = if pc & (PAGE_SIZE - 1) != PAGE_SIZE - 2 {
        // SAFETY: the fetch contract retains foreign RAM and every sampled address passes outer-stage permission checks.
        unsafe { hpa.as_ptr::<u16>().add(1).read_volatile() }
    } else {
        // SAFETY: the fetch contract retains foreign RAM and every sampled address passes outer-stage permission checks.
        unsafe { fetch_u16(space, satp, pc.checked_add(2)?) }?
    };
    Some((u32::from(low) | (u32::from(high) << 16), 4))
}

/// Fetch an instruction as two checked halfwords; each half may require a
/// different VS leaf and G-stage page. No guest VA is a host pointer.
/// # Safety
/// `instruction_gpa`'s contract applies. Executable HPAs must additionally be
/// foreign-mutable host-readable RAM for the two volatile halfword loads.
/// Concurrent code modification can yield a mixed instruction; the decoder
/// must validate it without granting access beyond that VM's device policy.
pub unsafe fn fetch_u32(space: &impl ReadOnlyStage2, satp: usize, pc: usize) -> Option<u32> {
    if satp == 0 {
        // SAFETY: the fetch contract retains foreign RAM and every sampled address passes outer-stage permission checks.
        return unsafe { space.fetch_bare_u32(pc) };
    }
    if pc & 1 != 0 {
        return None;
    }
    let mut halves = [0u16; 2];
    for (index, half) in halves.iter_mut().enumerate() {
        // SAFETY: the fetch contract retains foreign RAM and every sampled address passes outer-stage permission checks.
        let gpa = unsafe { instruction_gpa(space, satp, pc.checked_add(index * 2)?) }?;
        let hpa = space.translate(gpa)?;
        // SAFETY: the fetch contract retains foreign RAM and every sampled address passes outer-stage permission checks.
        *half = unsafe { hpa.as_ptr::<u16>().read_volatile() };
    }
    Some(u32::from(halves[0]) | (u32::from(halves[1]) << 16))
}
