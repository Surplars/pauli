//! Core timer owner adapter; SBI calls stay entirely within the ISA backend.
//! Ownership/Lock Ordering: one pinned physical CPU owns this compare cache.
//! Invariants: native IRQ masking never bypasses or invalidates cached endpoint state.
use pauli_core::{Result, arch::ArchTimer};
pub struct RiscvTimer(pub(crate) crate::timer::PhysicalTimer);
impl ArchTimer for RiscvTimer {
    fn new() -> Self {
        Self(crate::timer::PhysicalTimer::new())
    }
    fn now() -> u64 {
        crate::host::timer::read_time()
    }
    unsafe fn arm(&mut self, deadline: u64) -> Result<()> {
        // SAFETY: core grants exclusive IRQ-masked TIME endpoint ownership.
        unsafe { self.0.program(deadline) }.map_err(|_| pauli_core::HvError::Fault)?;
        // Rearm SSIP/STIP only. External delivery belongs to the board's IRQ
        // owner; setting a worker's compare must not also enable its SEIP.
        crate::host::irq::enable_wake_sources(false);
        Ok(())
    }
    unsafe fn disarm(&mut self) -> Result<()> {
        // SAFETY: matching endpoint owner before native ownership handoff.
        unsafe { self.arm(u64::MAX) }
    }
}
