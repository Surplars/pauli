//! Scoped base/AIA/file residency adapter; file 0 is never loaned to a guest.
//! Ownership/Lock Ordering: native owner -> short VM IRQ lock, no device I/O lock.
//! Invariants: put both banks before control, retirement, migration or owner idle.
use crate::{irq_backend::RiscvIrqChip, vcpu::VirtualCpu};
use pauli_core::{Result, arch::Resident};
pub struct RiscvResident {
    pub(crate) bank: crate::resident::Resident,
    #[cfg(feature = "aia-guest-file")]
    file: Option<crate::aia::hardware::Cache>,
}
impl Resident<VirtualCpu> for RiscvResident {
    unsafe fn new() -> Self {
        Self {
            bank: crate::resident::Resident::default(),
            #[cfg(feature="aia-guest-file")]
            // SAFETY: this scoped owner is IRQ-masked until put/drop.
            file: unsafe { crate::aia::hardware::Cache::acquire() },
        }
    }
    fn put(&mut self) {
        self.bank.put();
    }
    fn load_interrupts(&mut self, _cpu: &mut VirtualCpu, _chip: &mut RiscvIrqChip) -> Result<()> {
        #[cfg(feature = "aia-guest-file")]
        if let (Some(file), RiscvIrqChip::Advanced(chip)) = (&mut self.file, _chip) {
            // SAFETY: the core owns this pinned vCPU, and serializes its file inbox here.
            unsafe {
                if file.bound() {
                    file.merge(chip.take_pending(_cpu.index() as usize));
                } else {
                    file.load(_cpu, chip.take_file(_cpu.index() as usize));
                }
            }
        }
        Ok(())
    }
    fn put_interrupts(&mut self, _cpu: &mut VirtualCpu, _chip: &mut RiscvIrqChip) -> Result<()> {
        self.bank.put();
        #[cfg(feature = "aia-guest-file")]
        if let (Some(file), RiscvIrqChip::Advanced(chip)) = (&mut self.file, _chip) {
            if file.bound() {
                // SAFETY: guest exited, base bank put, same IRQ-masked owner and file.
                let state = unsafe { file.put(_cpu) };
                chip.put_file(_cpu.index() as usize, state);
            }
        }
        Ok(())
    }
}
