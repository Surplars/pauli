//! Integer, VS, FP and AIA snapshots for one exclusively owned vCPU.
//! Ownership/Lock Ordering: off-CPU owner mutates; no lock survives entry.
//! Invariants: trusted policy fields are not writable guest CSR snapshots.

use crate::{VS_OS_EXCEPTIONS, VS_PAGE_FAULTS, aia, fp};
/// Integer/VS state, distinct from a native task context. Execution PC comes
/// from HS sepc; vsepc remains the guest's own exception-return PC.
#[repr(C)]
pub struct VCpuContext {
    pub x: [usize; 32],
    pub pc: usize,
    pub vsstatus: usize,
    pub vsatp: usize,
    pub vstvec: usize,
    pub vsscratch: usize,
    pub vsepc: usize,
    pub vscause: usize,
    pub vstval: usize,
    pub vsie: usize,
    pub vsip: usize,
    pub hgatp: usize,
    pub exit_cause: usize,
    pub exit_value: usize,
    pub exit_gpa_shifted: usize,
    pub exit_instruction: usize,
    pub exit_hstatus: usize,
    // Trusted transition scratch, private to this vCPU while running.
    pub(crate) host_sp: usize,
    // These supervisor CSRs have no separate VS aliases and must be swapped.
    pub senvcfg: usize,
    pub scounteren: usize,
    // Timer/external pending are host-owned, not guest-writable CSR bits.
    pub hvip: usize,
    // Host-owned policy, not a guest-writable CSR. Append to preserve offsets.
    pub(crate) hedeleg: usize,
    // Trusted return privilege, initialized VS and updated from exit SPVP.
    pub(crate) resume_vs: usize,
    pub(crate) guest_os: bool,
    // Opt-in virtual IRQ delegation, independent of guest exception policy.
    pub(crate) hideleg: usize,
    // Full FLEN=64 bank, including NaN boxing and fcsr exception/rounding state.
    // Private enable policy is never inferred from guest-writable vsstatus.FS.
    pub fp: fp::FloatState,
    pub(crate) fp_enabled: usize,
    // Lifetime token and assembly plan, never supplied by guest CSR state.
    pub(crate) tlb_epoch: u32,
    pub(crate) tlb_vcpu: u8,
    pub(crate) tlb_flags: usize,
    // AIA availability is discovered, never inferred from a build.
    pub(crate) aia: aia::GuestState,
    // Provisioning policy, not a guest-controlled CSR or a hardware file index.
    pub(crate) virtual_aia: bool,
    // Trusted transient binding, never restored from a guest CSR or image.
    // Assembly offsets are derived from this single layout, not duplicated literals.
    pub(crate) guest_file: u8,
}
impl VCpuContext {
    pub const fn new() -> Self {
        Self {
            x: [0; 32],
            pc: 0,
            vsstatus: 0,
            vsatp: 0,
            vstvec: 0,
            vsscratch: 0,
            vsepc: 0,
            vscause: 0,
            vstval: 0,
            vsie: 0,
            vsip: 0,
            hgatp: 0,
            exit_cause: 0,
            exit_value: 0,
            exit_gpa_shifted: 0,
            exit_instruction: 0,
            exit_hstatus: 0,
            host_sp: 0,
            senvcfg: 0,
            scounteren: 0,
            hvip: 0,
            hedeleg: 0,
            resume_vs: 1,
            guest_os: false,
            hideleg: 1 << 6,
            fp: fp::FloatState::new(),
            fp_enabled: 0,
            tlb_epoch: 0,
            tlb_vcpu: 0,
            tlb_flags: 0,

            aia: aia::GuestState::new(),
            virtual_aia: false,
            guest_file: 0,
        }
    }

    /// Opt in to guest-owned page fault handling. Default interception is kept
    /// for existing bare guests. Invalid guest vectors still pass G-stage and
    /// cannot turn a delegated exception into a host instruction fetch.
    pub fn set_page_fault_delegation(&mut self, enabled: bool) {
        self.hedeleg = if enabled { VS_PAGE_FAULTS } else { 0 };
    }

    pub const fn page_faults_delegated(&self) -> bool {
        self.hedeleg & VS_PAGE_FAULTS == VS_PAGE_FAULTS
    }

    /// Opt in to guest OS exception handling and VS/VU privilege restoration.
    /// The default bare-guest profile still rejects VU-origin exits.
    pub fn enable_guest_os(&mut self) {
        self.hedeleg = VS_OS_EXCEPTIONS;
        self.guest_os = true;
        // Match the SBI boot environment: Linux's vDSO reads TIME in VU.
        // hcounteren opens the outer gate; scounteren must also permit TM.
        // Set this once at provisioning, not on entry: later guest writes
        // remain private context. Cycle/instret stay denied by hcounteren.
        self.scounteren = 1 << 1;
    }

    /// Start with software IMSIC traps on a discovered Ssaia host. A dedicated
    /// owner may later bind a physical file; guests never select host files.
    pub fn enable_virtual_aia(&mut self) -> Result<(), crate::Error> {
        if !cfg!(feature = "aia") || !aia::available() {
            return Err(crate::Error::Unsupported);
        }
        self.virtual_aia = true;
        Ok(())
    }

    /// Opt in once when provisioning a vCPU, after firmware capability discovery.
    /// Boot FS stays Off: the guest OS owns its own lazy userspace FP policy.
    pub fn enable_floating_point(&mut self) -> Result<(), crate::Error> {
        if !fp::available() {
            return Err(crate::Error::Unsupported);
        }
        self.fp_enabled = 1;
        Ok(())
    }
}
