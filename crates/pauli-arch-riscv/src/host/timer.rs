//! Native TIME counter/SBI endpoint, with no RTOS tick or scheduling policy.
//! Ownership/Lock Ordering: the physical dispatcher is the sole compare writer.
//! Invariants: guest TIME never invokes this endpoint directly; absolute deadlines persist.

pub fn read_time() -> u64 {
    let now;
    // SAFETY: the board's RV64 timebase contract includes an accessible TIME counter.
    unsafe {
        core::arch::asm!("rdtime {}", out(reg) now, options(nomem, nostack));
    }
    now
}
// Keep the per-owner probe CSR at an exact instruction address. A delegated
// illegal-instruction exception there returns false in the normal function ABI.
core::arch::global_asm!(
    ".pushsection .text",
    ".balign 4",
    ".global pauli_probe_stimecmp",
    ".type pauli_probe_stimecmp, @function",
    "pauli_probe_stimecmp:",
    "csrw {csr}, a0",
    "li a0, 1",
    "ret",
    ".global pauli_probe_stimecmp_denied",
    "pauli_probe_stimecmp_denied:",
    "li a0, 0",
    "ret",
    ".size pauli_probe_stimecmp, .-pauli_probe_stimecmp",
    ".popsection",
    csr = const crate::bits::CSR_STIMECMP,
);
unsafe extern "C" {
    fn pauli_probe_stimecmp(compare: u64) -> usize;
    fn pauli_probe_stimecmp_denied();
}

pub(crate) unsafe fn probe_disarm_stimecmp() -> bool {
    // SAFETY: the probing hart observed the local H gates, installed its native
    // exception vector, and owns its compare with local interrupts masked.
    unsafe { pauli_probe_stimecmp(u64::MAX) != 0 }
}

pub(super) fn recover_compare_probe(pc: &mut usize) -> bool {
    if *pc != pauli_probe_stimecmp as *const () as usize {
        return false;
    }
    *pc = pauli_probe_stimecmp_denied as *const () as usize;
    true
}

pub fn set_sbi_timer(deadline: u64) -> super::sbi::SbiRet {
    if crate::local_sstc_available() {
        // SAFETY: this hart completed its own guarded compare write before dispatch.
        unsafe {
            core::arch::asm!(
                "csrw {csr}, {val}",
                csr = const crate::bits::CSR_STIMECMP,
                val = in(reg) deadline,
                options(nostack)
            );
        }
        super::sbi::SbiRet { error: 0, value: 0 }
    } else {
        super::sbi::try_set_timer(deadline)
    }
}
