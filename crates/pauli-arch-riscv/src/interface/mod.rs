//! Native implementation of the architecture-neutral core contracts.
//! Ownership/Lock Ordering: one IRQ-masked pinned owner; no global VM registry.
//! Invariants: private static arenas and immutable G-stage authority outlive execution.
use crate::{Error, GStagePageTable};
use pauli_core::{HvError, Result, arch::HvArch};
mod cpu;
mod resident;
pub mod smp;
mod timer;
pub use resident::RiscvResident;
pub use timer::RiscvTimer;
pub struct RiscvHvArch;
pub fn error(error: Error) -> HvError {
    match error {
        Error::Unsupported => HvError::Unsupported,
        Error::Busy => HvError::Busy,
        Error::InvalidVcpu => HvError::InvalidVcpu,
        Error::InvalidImage => HvError::InvalidImage,
        Error::InvalidDeadline | Error::InvalidState => HvError::InvalidState,
        Error::TimerUnavailable | Error::HostTransition => HvError::Fault,
    }
}
impl HvArch for RiscvHvArch {
    type Vcpu = crate::vcpu::VirtualCpu;
    type Space = GStagePageTable;
    type Irq = crate::irq_backend::RiscvIrqChip;
    type Timer = RiscvTimer;
    type Smp = smp::RiscvSmp;
    type IrqMask = crate::host::irq::RiscvIrq;
    fn init(_layout: pauli_config::GuestBoardLayout) -> Result<()> {
        if crate::available() {
            Ok(())
        } else {
            Err(HvError::Unsupported)
        }
    }
    fn write_linux_dtb(
        config: &pauli_config::VmConfig,
        layout: pauli_config::GuestBoardLayout,
        hz: u32,
        args: &str,
        out: &mut [u8],
    ) -> Result<usize> {
        pauli_config::guest_dtb::riscv_configured(config, layout, hz, args, false, out)
            .map_err(|_| HvError::InvalidImage)
    }
    fn guest_sstc(dtb: &[u8], vcpus: u8) -> Result<bool> {
        pauli_config::guest_dtb::riscv_sstc(dtb, vcpus).map_err(|_| HvError::InvalidImage)
    }
    fn validate_control_events(
        dtb: &[u8],
        config: &pauli_config::VmConfig,
        layout: pauli_config::GuestBoardLayout,
    ) -> Result<()> {
        pauli_config::guest_dtb::validate_control_events(dtb, config, layout)
            .map_err(|_| HvError::InvalidImage)
    }
    fn sstc_available_for_owners(owners: usize) -> bool {
        crate::sstc_available_for_owners(owners)
    }
    fn write_extended_linux_dtb(
        config: &pauli_config::VmConfig,
        layout: pauli_config::GuestBoardLayout,
        hz: u32,
        args: &str,
        out: &mut [u8],
    ) -> Result<usize> {
        pauli_config::guest_dtb::riscv_configured(config, layout, hz, args, true, out)
            .map_err(|_| HvError::InvalidImage)
    }
    fn publish_code() {
        // SAFETY: this native architecture contract includes Zifencei and retained image publication.
        unsafe {
            crate::entry::publish_guest_code();
        }
    }
}
