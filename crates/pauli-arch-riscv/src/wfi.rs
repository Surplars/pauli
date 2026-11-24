//! Narrow WFI emulation. An arbitrary virtual-instruction trap must never be
//! skipped as idle. stval may supply instruction bits or legally be zero;
//! the latter needs a checked two-stage fetch in bare or Sv39 VS mode.
//! Ownership/Lock Ordering: exclusive stopped vCPU, retained instruction backing.
//! Invariants: only a validated VS WFI can advance PC or park a vCPU.
use super::{GStagePageTable, VCpuContext, VmExit};

pub const WFI: usize = 0x1050_0073;

/// Identify a real VS WFI exit. Unknown/nonzero stval is rejected, not fetched
/// over. Virtual U-mode WFI is not emulated by this VS-only backend.
///
/// # Safety
/// If stval is zero, tables and executable backing must satisfy
/// `vstage::fetch_u32`'s host-readable immutable RAM contract.
pub unsafe fn is_wfi(ctx: &VCpuContext, space: &GStagePageTable, exit: VmExit) -> bool {
    if !exit.from_guest
        || exit.cause != VmExit::VIRTUAL_INSTRUCTION
        || exit.pc != ctx.pc
        || ctx.exit_hstatus & (1 << 8) == 0
        || ctx.pc & 1 != 0
        || ctx.pc.checked_add(4).is_none()
    {
        return false;
    }
    if exit.value != 0 {
        return exit.value == WFI;
    }
    ctx.exit_hstatus & (1 << 5) == 0 // software walk supports little-endian VS
        // SAFETY: the caller retains checked executable/table RAM for this trusted trap snapshot.
        && unsafe { super::vstage::fetch_u32(space, ctx.vsatp, ctx.pc) } == Some(WFI as u32)
}
