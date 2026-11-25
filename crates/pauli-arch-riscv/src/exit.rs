//! Trusted HS exit snapshot; scalars are not guest-provided instruction authority.
//! Ownership/Lock Ordering: returned by value to the off-CPU owner; no locks.
//! Invariants: htval is GPA>>2; PC is sepc, distinct from the guest's vsepc.

#[derive(Clone, Copy, Debug)]
pub struct VmExit {
    pub cause: usize,
    pub value: usize,
    pub gpa_shifted: usize,
    pub instruction: usize,
    pub pc: usize,
    pub from_guest: bool,
}
impl VmExit {
    #[cfg(any(target_arch = "riscv64", test))]
    pub(crate) fn source(
        cause: usize,
        action: Option<crate::sbi::SbiAction>,
        wfi: bool,
        mmio: bool,
        native_sstc: bool,
    ) -> pauli_core::arch::ExitSource {
        use pauli_core::arch::ExitSource;
        match cause {
            Self::SUPERVISOR_TIMER if native_sstc => ExitSource::SstcTimer,
            Self::SUPERVISOR_TIMER => ExitSource::Timer,
            Self::SUPERVISOR_EXTERNAL => ExitSource::External,
            Self::SUPERVISOR_SOFTWARE => ExitSource::Software,
            Self::VS_ECALL if matches!(action, Some(crate::sbi::SbiAction::SetTimer { .. })) => {
                ExitSource::SbiTimer
            }
            Self::VS_ECALL => ExitSource::SupervisorCall,
            _ if wfi => ExitSource::Wfi,
            _ if mmio => ExitSource::Mmio,
            _ => ExitSource::Other,
        }
    }

    pub const VS_ECALL: usize = 10;
    pub const SUPERVISOR_SOFTWARE: usize = (1usize << 63) | 1;
    pub const SUPERVISOR_TIMER: usize = (1usize << 63) | 5;
    pub const SUPERVISOR_EXTERNAL: usize = (1usize << 63) | 9;
    pub const VIRTUAL_INSTRUCTION: usize = 22;
    pub const INSTRUCTION_PAGE_FAULT: usize = 12;
    pub const LOAD_PAGE_FAULT: usize = 13;
    pub const STORE_PAGE_FAULT: usize = 15;
    pub const GUEST_LOAD_PAGE_FAULT: usize = 21;
    pub const GUEST_STORE_PAGE_FAULT: usize = 23;
    pub const GUEST_INSTRUCTION_PAGE_FAULT: usize = 20;
}

#[cfg(test)]
mod tests {
    use super::VmExit;
    use pauli_core::arch::ExitSource::*;

    #[test]
    fn decoded_wfi_and_mmio_are_distinct_from_failed_emulation() {
        for (cause, wfi, mmio, source) in [
            (VmExit::VIRTUAL_INSTRUCTION, true, false, Wfi),
            (VmExit::VIRTUAL_INSTRUCTION, false, false, Other),
            (VmExit::GUEST_LOAD_PAGE_FAULT, false, false, Other),
            (VmExit::GUEST_STORE_PAGE_FAULT, false, true, Mmio),
            (VmExit::VS_ECALL, false, false, SupervisorCall),
            (VmExit::SUPERVISOR_TIMER, false, false, Timer),
            (VmExit::SUPERVISOR_EXTERNAL, false, false, External),
            (VmExit::SUPERVISOR_SOFTWARE, false, false, Software),
        ] {
            assert_eq!(VmExit::source(cause, None, wfi, mmio, false), source);
        }
        use crate::sbi::SbiAction::{Resume, SetTimer};
        assert_eq!(
            VmExit::source(
                VmExit::VS_ECALL,
                Some(SetTimer { deadline: 40 }),
                false,
                false,
                true
            ),
            SbiTimer
        );
        assert_eq!(
            VmExit::source(VmExit::VS_ECALL, Some(Resume), false, false, true),
            SupervisorCall
        );
        assert_eq!(
            VmExit::source(VmExit::SUPERVISOR_TIMER, None, false, false, true),
            SstcTimer
        );
    }
}
