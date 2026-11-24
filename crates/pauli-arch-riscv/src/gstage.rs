//! Fixed-storage Sv39x4 translation. Root storage is 16 KiB, not a normal
//! 512-entry Sv39 table. Only inactive, exclusively owned tables may be edited.
//! Ownership/Lock Ordering: inactive exclusive table owner; immutable during execution.
//! Invariants: child pointers stay in the private lower-table arena; ordinary leaves enforce W^X.
use crate::vstage::ReadOnlyStage2;
#[cfg(any(target_arch = "riscv64", test))]
mod space;
use pauli_hal::addr::{MemPerms, MmError, PAGE_SIZE, PhysAddr};

const V: u64 = 1;
const R: u64 = 2;
const W: u64 = 4;
const X: u64 = 8;
const U: u64 = 16;
const A: u64 = 1 << 6;
const AD: u64 = A | (1 << 7);
const PPN_MASK: u64 = (1 << 44) - 1;
pub const ROOT_SIZE: usize = 16384;

#[repr(C, align(16384))]
pub struct GStageRoot {
    entries: [u64; 2048],
}

impl ReadOnlyStage2 for GStagePageTable {
    fn translate(&self, gpa: PhysAddr) -> Option<PhysAddr> {
        GStagePageTable::translate(self, gpa)
    }
    fn translate_readable(&self, gpa: PhysAddr) -> Option<PhysAddr> {
        GStagePageTable::translate_readable(self, gpa)
    }
    fn is_executable(&self, gpa: PhysAddr) -> bool {
        GStagePageTable::is_executable(self, gpa)
    }
    unsafe fn fetch_bare_u32(&self, pc: usize) -> Option<u32> {
        // SAFETY: the caller retains the executable foreign RAM for this fetch.
        unsafe { GStagePageTable::fetch_bare_u32(self, pc) }
    }
}
impl GStageRoot {
    pub const fn new() -> Self {
        Self { entries: [0; 2048] }
    }
}
#[repr(C, align(4096))]
pub struct GStageTable {
    entries: [u64; 512],
}
impl GStageTable {
    pub const fn new() -> Self {
        Self { entries: [0; 512] }
    }
}

const _: () = {
    assert!(core::mem::size_of::<GStageRoot>() == ROOT_SIZE);
    assert!(core::mem::align_of::<GStageRoot>() == ROOT_SIZE);
    assert!(core::mem::size_of::<GStageTable>() == PAGE_SIZE);
};

pub struct GStagePageTable {
    root: &'static mut GStageRoot,
    lower: &'static mut [GStageTable],
    used: usize,
}
impl GStagePageTable {
    /// # Safety
    /// The storage must be identity-mapped, private to this VM, and inactive.
    /// The returned tables and every mapped frame must outlive guest execution.
    /// Mapped HPAs must be assigned RAM/device frames, not host-private memory.
    pub unsafe fn new_identity(
        root: &'static mut GStageRoot,
        lower: &'static mut [GStageTable],
    ) -> Self {
        root.entries.fill(0);
        for table in lower.iter_mut() {
            table.entries.fill(0);
        }
        Self {
            root,
            lower,
            used: 0,
        }
    }
    pub fn root_addr(&self) -> PhysAddr {
        PhysAddr::new(self.root as *const _ as usize)
    }
    pub fn hgatp_value(&self) -> usize {
        // Untagged root template. The physical owner binds its own VMID just
        // before entry; raw contexts without an epoch retain full invalidation.
        (8usize << 60) | (self.root_addr().raw() >> 12)
    }
    fn indices(gpa: usize) -> Result<[usize; 3], MmError> {
        if gpa >= (1usize << 41) {
            return Err(MmError::InvalidRegion);
        }
        Ok([(gpa >> 12) & 511, (gpa >> 21) & 511, (gpa >> 30) & 2047])
    }
    fn child(&self, pte: u64) -> Result<*mut GStageTable, MmError> {
        if pte & (V | R | W | X) != V {
            return Err(MmError::InvalidRegion);
        }
        let address = (((pte >> 10) & PPN_MASK) as usize) << 12;
        let base = self.lower.as_ptr() as usize;
        let offset = address.checked_sub(base).ok_or(MmError::InvalidRegion)?;
        if offset % PAGE_SIZE != 0 || offset / PAGE_SIZE >= self.used {
            return Err(MmError::InvalidRegion);
        }
        Ok(address as *mut GStageTable)
    }
    /// Adds one 4 KiB leaf. No heap/frame allocator, live remapping or W+X.
    /// On table exhaustion, zeroed intermediate tables remain reserved for
    /// later use; no leaf is installed and no external allocation is leaked.
    pub fn map_page(
        &mut self,
        gpa: PhysAddr,
        hpa: PhysAddr,
        perms: MemPerms,
    ) -> Result<(), MmError> {
        let vpn = Self::indices(gpa.raw())?;
        if !gpa.is_aligned(PAGE_SIZE) || !hpa.is_aligned(PAGE_SIZE) {
            return Err(MmError::InvalidAlignment);
        }
        if hpa.raw() >= (1usize << 56) {
            return Err(MmError::InvalidRegion);
        }
        if perms.is_empty()
            || (perms.contains(MemPerms::WRITE)
                && (!perms.contains(MemPerms::READ) || perms.contains(MemPerms::EXECUTE)))
        {
            return Err(MmError::InvalidPermissions);
        }
        let mut slot = &mut self.root.entries[vpn[2]] as *mut u64;
        for level in (1..=2).rev() {
            // SAFETY: the exclusive inactive owner retains the checked table slot in its private arena.
            let mut pte = unsafe { slot.read() };
            if pte & V == 0 {
                if self.used == self.lower.len() {
                    return Err(MmError::OutOfMemory);
                }
                let address = &mut self.lower[self.used] as *mut _ as usize;
                self.used += 1;
                pte = V | (((address >> 12) as u64) << 10);
                // SAFETY: the exclusive inactive owner retains the checked table slot in its private arena.
                unsafe { slot.write(pte) };
            }
            let child = self.child(pte)?;
            // SAFETY: the exclusive inactive owner retains the checked table slot in its private arena.
            slot = unsafe { &mut (*child).entries[vpn[level - 1]] };
        }
        // SAFETY: the exclusive inactive owner retains the checked table slot in its private arena.
        if unsafe { slot.read() } & V != 0 {
            return Err(MmError::AlreadyMapped);
        }
        // G-stage checks every access as U-mode, even for a VS-mode guest.
        let mut bits = V | U | AD;
        if perms.contains(MemPerms::READ) {
            bits |= R;
        }
        if perms.contains(MemPerms::WRITE) {
            bits |= W;
        }
        if perms.contains(MemPerms::EXECUTE) {
            bits |= X;
        }
        // SAFETY: the exclusive inactive owner retains the checked table slot in its private arena.
        unsafe { slot.write(bits | (((hpa.raw() >> 12) as u64) << 10)) };
        Ok(())
    }
    /// Assign one private 2 MiB RAM extent to a guest OS. The guest manages
    /// its own text/data protections; ordinary `map_page` still enforces W^X.
    /// # Safety
    /// The entire aligned HPA extent must be exclusively assigned, retained
    /// guest RAM, excluded from host allocation. Never use for shared RAM,
    /// host/kernel memory or MMIO. Edit only while the VM is inactive.
    pub unsafe fn map_private_ram_2m(
        &mut self,
        gpa: PhysAddr,
        hpa: PhysAddr,
    ) -> Result<(), MmError> {
        const SIZE: usize = 1 << 21;
        let vpn = Self::indices(gpa.raw())?;
        if !gpa.is_aligned(SIZE) || !hpa.is_aligned(SIZE) {
            return Err(MmError::InvalidAlignment);
        }
        if hpa.raw() >= (1usize << 56) {
            return Err(MmError::InvalidRegion);
        }
        let mut pte = self.root.entries[vpn[2]];
        if pte & V == 0 {
            if self.used == self.lower.len() {
                return Err(MmError::OutOfMemory);
            }
            let address = &mut self.lower[self.used] as *mut _ as usize;
            self.used += 1;
            pte = V | (((address >> 12) as u64) << 10);
            self.root.entries[vpn[2]] = pte;
        }
        let table = self.child(pte)?;
        // SAFETY: the exclusive inactive owner retains the checked table slot in its private arena.
        let slot = unsafe { &mut (*table).entries[vpn[1]] };
        if *slot & V != 0 {
            return Err(MmError::AlreadyMapped);
        }
        *slot = V | R | W | X | U | AD | (((hpa.raw() >> 12) as u64) << 10);
        Ok(())
    }

    /// Assign one private 1 GiB RAM extent to a guest OS. The guest manages
    /// its own text/data protections; ordinary `map_page` still enforces W^X.
    /// # Safety
    /// The entire aligned HPA extent must be exclusively assigned, retained
    /// guest RAM, excluded from host allocation. Never use for shared RAM,
    /// host/kernel memory or MMIO. Edit only while the VM is inactive.
    pub unsafe fn map_private_ram_1g(
        &mut self,
        gpa: PhysAddr,
        hpa: PhysAddr,
    ) -> Result<(), MmError> {
        const SIZE: usize = 1 << 30;
        let vpn = Self::indices(gpa.raw())?;
        if !gpa.is_aligned(SIZE) || !hpa.is_aligned(SIZE) {
            return Err(MmError::InvalidAlignment);
        }
        if hpa.raw() >= (1usize << 56) {
            return Err(MmError::InvalidRegion);
        }
        let slot = &mut self.root.entries[vpn[2]];
        if *slot & V != 0 {
            return Err(MmError::AlreadyMapped);
        }
        *slot = V | R | W | X | U | AD | (((hpa.raw() >> 12) as u64) << 10);
        Ok(())
    }

    /// Resolve trusted G-stage leaves, including private-RAM superpages.
    fn leaf(&self, gpa: PhysAddr) -> Option<(u64, usize)> {
        let vpn = Self::indices(gpa.raw()).ok()?;
        let mut pte = self.root.entries[vpn[2]];
        for level in (0..=2).rev() {
            if pte & V == 0 {
                return None;
            }
            if pte & (R | X) != 0 {
                let offset_bits = 12 + 9 * level;
                let mask = (1usize << offset_bits) - 1;
                if ((((pte >> 10) & PPN_MASK) as usize) << 12) & mask != 0 {
                    return None;
                }
                return Some((pte, mask));
            }
            if level == 0 {
                return None;
            }
            let child = self.child(pte).ok()?;
            // SAFETY: the exclusive inactive owner retains the checked table slot in its private arena.
            pte = unsafe { (*child).entries[vpn[level - 1]] };
        }
        None
    }
    pub fn translate(&self, gpa: PhysAddr) -> Option<PhysAddr> {
        let (leaf, mask) = self.leaf(gpa)?;
        Some(PhysAddr::new(
            ((((leaf >> 10) & PPN_MASK) as usize) << 12) | (gpa.raw() & mask),
        ))
    }
    pub fn is_executable(&self, gpa: PhysAddr) -> bool {
        self.leaf(gpa)
            .is_some_and(|(leaf, _)| leaf & (V | X | U | A) == (V | X | U | A))
    }

    /// Translate an implicit guest page-table read, without treating X as R.
    /// The outer mapping is trusted/immutable, but a VS PTE's GPA is not.
    pub fn translate_readable(&self, gpa: PhysAddr) -> Option<PhysAddr> {
        let (leaf, _) = self.leaf(gpa)?;
        if leaf & (V | R | U | A) != (V | R | U | A) {
            return None;
        }
        self.translate(gpa)
    }

    /// Fetch a 32-bit instruction in the initial bare VS address space. Each
    /// halfword is separately permission-checked/translated: a C-aligned
    /// 32-bit instruction may straddle two independently mapped pages.
    /// Never dereference a raw guest PC as a host pointer.
    ///
    /// # Safety
    /// Every accessed executable HPA must be retained, identity-mapped,
    /// host-readable foreign RAM (not MMIO or aliased Rust references). A guest
    /// writer may race the volatile halfwords; callers must validate a sampled
    /// instruction, not assume an atomic code image. Not a VS page-table walker.
    pub unsafe fn fetch_bare_u32(&self, pc: usize) -> Option<u32> {
        if pc & 1 != 0 {
            return None;
        }
        let mut parts = [0u16; 2];
        for (index, part) in parts.iter_mut().enumerate() {
            let gpa = PhysAddr::new(pc.checked_add(index * 2)?);
            if !self.is_executable(gpa) {
                return None;
            }
            let hpa = self.translate(gpa)?;
            // SAFETY: the independently checked executable HPA is retained host-readable foreign RAM, not MMIO.
            *part = unsafe { hpa.as_ptr::<u16>().read_volatile() };
        }
        Some(u32::from(parts[0]) | (u32::from(parts[1]) << 16))
    }
}
