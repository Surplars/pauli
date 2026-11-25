//! Core interrupt-chip adapter; policy is selected once at VM construction.
//! Ownership/Lock Ordering: one VM IRQ owner; file transfers hold only this lock.
//! Invariants: IRQ source/context indices are validated by immutable VM wiring.
use crate::{
    aia::virtualized::VirtualAia, interrupt::VirtualInterruptController, plic::VirtualPlic,
};
use pauli_config::{GuestIrqConfig, GuestIrqController};
use pauli_core::{
    HvError, Result,
    arch::{IrqRegion, RegisterAccess, VirtualIrqChip},
};
pub enum RiscvIrqChip {
    Legacy(VirtualPlic),
    Advanced(VirtualAia),
}
impl VirtualIrqChip for RiscvIrqChip {
    fn create_configured(irq: GuestIrqConfig) -> Result<Self> {
        let cpus = usize::from(irq.vcpus);
        if !(1..=crate::MAX_VCPUS).contains(&cpus) {
            return Err(HvError::InvalidVcpu);
        }
        match irq.controller {
            GuestIrqController::Legacy { .. } => Ok(Self::Legacy(VirtualPlic::with_contexts(cpus))),
            GuestIrqController::Advanced { ids, .. } => {
                if !cfg!(feature = "aia") || !crate::aia::available() {
                    return Err(HvError::Unsupported);
                }
                Ok(Self::Advanced(VirtualAia::with_ids(cpus, ids)))
            }
        }
    }
    fn access(&mut self, region: IrqRegion, offset: usize, write: Option<usize>) -> Option<usize> {
        match (self, region) {
            (Self::Legacy(p), IrqRegion::Legacy) => match write {
                Some(value) => p.write_word(offset, value as u32).map(|()| 0),
                None => p.read_word(offset).map(|v| v as usize),
            },
            (Self::Advanced(a), IrqRegion::Wired) => a.aplic_access(offset, write),
            (Self::Advanced(a), IrqRegion::Message) => a.msi_access(offset, write),
            _ => None,
        }
    }
    fn register(&mut self, cpu: u8, access: RegisterAccess) -> Option<usize> {
        match self {
            Self::Advanced(a) => a
                .csr(
                    cpu as usize,
                    crate::aia::emulation::CsrAccess {
                        register: access.register,
                        select: access.select,
                        clear: access.clear,
                        set: access.set,
                        write: access.write,
                    },
                )
                .map(|(value, _)| value),
            Self::Legacy(_) => None,
        }
    }
    fn set_level(&mut self, source: u32, asserted: bool) {
        match self {
            Self::Legacy(p) => p.set_level(source, asserted),
            Self::Advanced(a) => a.set_level(source, asserted),
        }
    }
    fn pending(&self, cpu: u8) -> bool {
        match self {
            Self::Legacy(p) => p.external_pending_for(cpu as usize),
            Self::Advanced(a) => a.line(cpu as usize),
        }
    }
    fn quiesce(&mut self) -> Result<()> {
        match self {
            Self::Advanced(a) => {
                a.quiesce();
            }
            Self::Legacy(p) => {
                for source in 1..32 {
                    p.retire_source(source);
                }
            }
        }
        Ok(())
    }
    #[cfg(feature = "selftest")]
    fn file_stats(&self) -> Option<pauli_core::arch::IrqFileStats> {
        let Self::Advanced(chip) = self else {
            return None;
        };
        let h = chip.hardware();
        Some(pauli_core::arch::IrqFileStats {
            #[cfg(feature = "aia-guest-file")]
            loaded_vcpus: h.loaded_vcpus,
            #[cfg(not(feature = "aia-guest-file"))]
            loaded_vcpus: 0,
            loads: h.loads,
            puts: h.puts,
            merges: h.merges,
            injected: h.injected,
            bound: h.bound,
        })
    }
}
