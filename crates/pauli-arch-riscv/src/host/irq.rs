//! Local supervisor IRQ masking/restoration and idle wake primitives.
//! Ownership/Lock Ordering: save actual SIE before a lock; restore only after release.
//! Invariants: nested masks preserve the caller's original state; no global nesting counter.

use crate::bits;
pub struct RiscvIrq;
// SAFETY: SIE is cleared atomically; guards keep the matching token on this CPU.
unsafe impl pauli_hal::sync::LocalIrq for RiscvIrq {
    type Saved = usize;
    fn save() -> usize {
        let saved;
        // SAFETY: native HS/S-mode owner; immediate bit masks local supervisor IRQ delivery.
        unsafe {
            core::arch::asm!("csrrci {}, sstatus, {sie}", out(reg) saved, sie = const bits::SSTATUS_SIE, options(nostack));
        }
        saved
    }
    unsafe fn restore(saved: usize) {
        if saved & bits::SSTATUS_SIE != 0 {
            // SAFETY: LocalIrq contract supplies this CPU's matching saved state.
            unsafe {
                core::arch::asm!("csrsi sstatus, {sie}", sie = const bits::SSTATUS_SIE, options(nostack));
            }
        }
    }
}
pub fn clear_software_interrupt(#[cfg(feature = "selftest")] path: pauli_core::arch::NativeAckPath) {
    #[cfg(not(feature = "selftest"))]
    // SAFETY: SSIP is the HS-owned writable physical kick level, not guest VSSIP.
    unsafe {
        core::arch::asm!("csrci sip, {ssip}", ssip = const bits::SSIP, options(nostack));
    }
    #[cfg(feature = "selftest")]
    {
        let old: usize;
        // SAFETY: same physical ACK; one CSR transaction tests and clears the level.
        unsafe {
            core::arch::asm!("csrrci {}, sip, {ssip}", out(reg) old, ssip = const bits::SSIP, options(nostack));
        }
        crate::native_ack::RECEIPTS.observe(old, path, super::cpu_id, super::timer::read_time);
    }
}
/// Leave SSIP latched until the dispatcher can rescan published work. Masking
/// SSIE prevents retrapping; the next native wait restores local wake sources.
pub fn mask_software_interrupt() {
    // SAFETY: only this native owner changes its supervisor interrupt enables.
    unsafe {
        core::arch::asm!("csrci sie, {ssie}", ssie = const bits::SSIP, options(nostack));
    }
}
pub fn enable_wake_sources(external: bool) {
    let bits = bits::HOST_GUEST_INTERRUPTS | if external { bits::SEIP } else { 0 };
    // SAFETY: caller's native CPU owns sie; no guest state is modified.
    unsafe {
        core::arch::asm!("csrs sie, {}", in(reg) bits, options(nostack));
    }
}
pub fn mask_timer() {
    // SAFETY: masks a pending native timer until its dispatcher replaces the compare.
    unsafe {
        core::arch::asm!("csrc sie, {}", in(reg) bits::STIP, options(nostack));
    }
}
/// Enable the native timer as a WFI wake source without enabling global delivery.
pub fn unmask_timer() {
    // SAFETY: the physical owner changes only its local STIE bit.
    unsafe { core::arch::asm!("csrs sie, {}", in(reg) bits::STIP, options(nostack)) };
}
pub fn timer_pending() -> bool {
    let pending: usize;
    // SAFETY: read-only snapshot; this never acknowledges the local timer.
    unsafe { core::arch::asm!("csrr {}, sip", out(reg) pending, options(nostack)) };
    pending & bits::STIP != 0
}
/// # Safety
/// Native host vector installed; no business lock/resident guest bank may remain held.
pub unsafe fn enable() {
    // SAFETY: caller established a valid host trap vector and native stack.
    unsafe {
        core::arch::asm!("csrsi sstatus, {sie}", sie = const bits::SSTATUS_SIE, options(nostack));
    }
}
/// # Safety
/// The owner has armed wake sources and rechecked its queues after acknowledging SSIP.
pub unsafe fn wait() {
    // SAFETY: WFI lends only the native idle CPU; no locks or guest bank remain owned.
    unsafe {
        core::arch::asm!("wfi", options(nostack));
    }
}
