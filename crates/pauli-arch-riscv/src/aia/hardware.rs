//! A transient, owner-local cache in physical guest file 1. File 0 always
//! belongs to HS. No guest GPA maps this file and no physical device sends MSIs
//! here, so saving/clearing it cannot race an untracked physical producer.
//! Ownership/Lock Ordering: one IRQ-masked physical owner; put before any foreign control.
//! Invariants: file 0 is HS-only, file 1 is reserved and empty on reuse.
use super::{file_bank::OwnerFile, file_state, imsic::File};
use crate::host::aia_csr::hypervisor as csr;
use crate::vcpu::VirtualCpu;
use core::marker::PhantomData;
use pauli_config::ImsicIds;

const FILE: usize = 1;
const BIT: usize = 1 << FILE;
// Each owner probes and publishes its own file without enabling any other hart.
static OWNERS: [OwnerFile; crate::MAX_CPUS] = [const { OwnerFile::new() }; crate::MAX_CPUS];

/// Reserve exactly the profile we can save in full. Other sizes retain software
/// AIA, rather than exposing extra physical IDs and leaking them on reuse.
/// # Safety
/// Dedicated HS owner, H+Ssaia discovered, IRQs masked, before publishing jobs.
/// Firmware/board policy reserves guest file 1 exclusively for this cache.
pub unsafe fn init_owner(guest_ids: Option<usize>) -> bool {
    let local = &OWNERS[crate::host::cpu_id()];
    // A repeated boot probe cannot leave an earlier size advertised on failure.
    local.publish(None);
    let Some(ids) = guest_ids
        .and_then(ImsicIds::from_count)
        .filter(|_| super::available())
    else {
        return false;
    };
    // SAFETY: this IRQ-masked owner holds the discovered bank and exclusively reserved guest file 1.
    let old_enable = unsafe { csr::hgeie::read() };
    let old_status = unsafe { csr::hstatus::read() };
    if old_enable & BIT != 0 || old_status & super::VGEIN_MASK != 0 {
        return false;
    }
    // HGEIE is WARL: a set/read/restore discovers whether file 1 exists. Keep
    // it disabled afterwards; this stage returns files before owner idle WFI.
    // SAFETY: this IRQ-masked owner holds the discovered bank and exclusively reserved guest file 1.
    unsafe { csr::hgeie::write(old_enable | BIT) };
    let present = unsafe { csr::hgeie::read() } & BIT != 0;
    // SAFETY: this IRQ-masked owner holds the discovered bank and exclusively reserved guest file 1.
    unsafe { csr::hgeie::write(old_enable) };
    if !present {
        return false;
    }
    // Firmware/reset contents are unspecified. Establish the empty-file
    // invariant once here, and preserve it on every put/reuse afterwards.
    // SAFETY: this IRQ-masked owner holds the discovered bank and exclusively reserved guest file 1.
    let _ = file_state::save_clear(&mut unsafe { Window::new() }, ids);
    local.publish(Some(ids));
    true
}

/// Never Send: the file, optional VS bank and physical owner share a lifetime.
pub struct Cache {
    ids: ImsicIds,
    bound: bool,
    _local: PhantomData<*mut ()>,
}
impl Cache {
    /// # Safety
    /// Dedicated owner with IRQs masked until this cache is put and dropped.
    pub unsafe fn acquire(requested: ImsicIds) -> Option<Self> {
        if !OWNERS[crate::host::cpu_id()].acquire(requested) {
            return None;
        }
        Some(Self {
            ids: requested,
            bound: false,
            _local: PhantomData,
        })
    }
    pub fn bound(&self) -> bool {
        self.bound
    }
    /// # Safety
    /// This vCPU exclusively owns the resident bank; software has transferred
    /// its complete file under the VM device lock before calling load.
    pub unsafe fn load(&mut self, cpu: &mut VirtualCpu, file: File) {
        assert!(!self.bound);
        assert_eq!(file.ids(), self.ids);
        let external = file.line();
        // SAFETY: this IRQ-masked owner holds the discovered bank and exclusively reserved guest file 1.
        file_state::load(&mut unsafe { Window::new() }, file);
        cpu.bind_imsic(FILE as u8, external);
        self.bound = true;
    }
    /// # Safety
    /// Same owner/vCPU, after guest exit, pending taken from its locked inbox.
    pub unsafe fn merge(&mut self, pending: [u64; super::imsic::WORDS]) {
        assert!(self.bound);
        // SAFETY: this IRQ-masked owner holds the discovered bank and exclusively reserved guest file 1.
        file_state::merge(self.ids, pending, || unsafe { Window::new() });
    }
    /// # Safety
    /// Before any control reset, foreign context or owner dispatch. Caller must
    /// merge late inbox bits into this snapshot before publishing software mode.
    pub unsafe fn put(&mut self, cpu: &mut VirtualCpu) -> File {
        assert!(self.bound);
        // SAFETY: this IRQ-masked owner holds the discovered bank and exclusively reserved guest file 1.
        let file = file_state::save_clear(&mut unsafe { Window::new() }, self.ids);
        cpu.bind_imsic(0, file.line());
        self.bound = false;
        file
    }
}
impl Drop for Cache {
    fn drop(&mut self) {
        assert!(!self.bound, "put IMSIC before releasing the physical owner");
        OWNERS[crate::host::cpu_id()].publish(Some(self.ids));
    }
}

struct Window {
    status: usize,
    select: usize,
}
impl Window {
    unsafe fn new() -> Self {
        // SAFETY: this IRQ-masked owner holds the discovered bank and exclusively reserved guest file 1.
        let status = unsafe { csr::hstatus::read() };
        let select = unsafe { csr::vsiselect::read() };
        // SAFETY: this IRQ-masked owner holds the discovered bank and exclusively reserved guest file 1.
        unsafe { csr::hstatus::write((status & !super::VGEIN_MASK) | (FILE << 12)) };
        Self { status, select }
    }
}
impl file_state::Registers for Window {
    fn take(&mut self, select: usize) -> usize {
        // SAFETY: this IRQ-masked owner holds the discovered bank and exclusively reserved guest file 1.
        unsafe {
            csr::vsiselect::write(select);
            csr::vsireg::swap(0)
        }
    }
    fn set(&mut self, select: usize, bits: usize) {
        // SAFETY: this IRQ-masked owner holds the discovered bank and exclusively reserved guest file 1.
        unsafe {
            csr::vsiselect::write(select);
            csr::vsireg::set(bits);
        }
    }
    fn read(&mut self, select: usize) -> usize {
        // SAFETY: this IRQ-masked owner holds the discovered bank and exclusively reserved guest file 1.
        unsafe {
            csr::vsiselect::write(select);
            csr::vsireg::read()
        }
    }
    fn write(&mut self, select: usize, value: usize) {
        // SAFETY: this IRQ-masked owner holds the discovered bank and exclusively reserved guest file 1.
        unsafe {
            csr::vsiselect::write(select);
            csr::vsireg::write(value);
        }
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: this IRQ-masked owner holds the discovered bank and exclusively reserved guest file 1.
        unsafe {
            csr::vsiselect::write(self.select);
            csr::hstatus::write(self.status);
        }
    }
}
