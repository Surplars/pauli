//! RV64 supervisor host support; board discovery and resources stay outside this crate.
//! Ownership/Lock Ordering: tp holds the boot-installed physical CPU index; IRQs are local.
//! Invariants: initialize topology before dispatch; no native task/RTOS context exists.

pub mod aia_csr;
pub mod aplic;
pub mod imsic;
pub mod irq;
pub mod plic;
pub mod sbi;
pub mod smp;
pub mod sv39;
pub mod timer;
pub mod trap;

pub fn cpu_id() -> usize {
    let cpu;
    // SAFETY: tp is the HS-owned index, saved/restored around every guest entry.
    unsafe {
        core::arch::asm!("mv {}, tp", out(reg) cpu, options(nomem, nostack));
    }
    cpu
}
/// # Safety
/// Boot or HSM startup only, with IRQs masked before touching per-owner state.
/// Each participating hart must receive a unique index below MAX_CPUS.
pub unsafe fn install_cpu_id(cpu: usize) {
    // SAFETY: native firmware startup owns tp and has not entered a guest.
    unsafe {
        core::arch::asm!("mv tp, {}", in(reg) cpu, options(nostack));
    }
}
