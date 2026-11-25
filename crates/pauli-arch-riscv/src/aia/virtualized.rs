//! APLIC routes stay under the device owner. IMSIC banks can use a separate
//! short lock, so native block service cannot stall a file put or MSI drain.
//! Ownership/Lock Ordering: VM device owner, then short file-bank lock.
//! Invariants: guest offsets and MSI targets cannot address host files or peer VMs.
pub use super::file_bank::HardwareStats;
use super::registers::{
    APLIC_SIZE, MSI_FILE_SIZE, PRIORITY_FIRST, PRIORITY_LAST, PRIORITY_LAST_EVEN, SIREG, STOPEI,
};
use super::{
    aplic::Aplic,
    emulation::CsrAccess,
    file_bank::{Bank, Storage},
    imsic::File,
};
use pauli_config::ImsicIds;
pub struct VirtualAia {
    ids: ImsicIds,
    aplic: Aplic,
    files: Storage,
    cpus: usize,
    pub claims: u64,
    pub csr_accesses: u64,
}
impl VirtualAia {
    pub const fn new(cpus: usize) -> Self {
        Self::with_ids(cpus, ImsicIds::Ids255)
    }
    pub const fn with_ids(cpus: usize, ids: ImsicIds) -> Self {
        Self {
            ids,
            aplic: Aplic::new(),
            files: Storage::Inline(Bank::with_ids(cpus, ids)),
            cpus,
            claims: 0,
            csr_accesses: 0,
        }
    }
    /// The board lends this bank until every producer and vCPU owner has
    /// joined. A guest MMIO reset is never permission to reset a shared bank.
    pub fn with_shared(cpus: usize, files: &'static pauli_hal::sync::SpinLock<Bank>) -> Self {
        let ids = {
            let bank = files.lock();
            assert_eq!(bank.cpus(), cpus);
            bank.ids()
        };
        Self {
            files: Storage::Shared(files),
            ..Self::with_ids(cpus, ids)
        }
    }
    pub const fn ids(&self) -> ImsicIds {
        self.ids
    }
    pub fn hardware(&self) -> HardwareStats {
        self.files.with(Bank::hardware)
    }
    pub fn set_level(&mut self, source: u32, high: bool) {
        self.set_levels(&[(source, high)]);
    }
    /// Device-owner batch: update the wires first, then lock the file bank only
    /// once for forwarding. No guest can change APLIC routes in this interval.
    pub fn set_levels(&mut self, levels: &[(u32, bool)]) {
        for &(source, high) in levels {
            self.aplic.set_level(source, high);
        }
        self.files
            .with_mut(|bank| self.aplic.forward(&mut bank.files[..self.cpus]));
    }
    pub fn line(&self, cpu: usize) -> bool {
        self.files.with(|bank| bank.line(cpu))
    }
    /// Transfer the complete file, not a copy that two owners could update.
    pub fn take_file(&mut self, cpu: usize) -> File {
        self.files.with_mut(|bank| bank.take_file(cpu))
    }
    pub fn take_pending(&mut self, cpu: usize) -> [u64; super::imsic::WORDS] {
        self.files.with_mut(|bank| bank.take_pending(cpu))
    }
    /// Producers may inject while hardware is being saved/cleared. Merge that
    /// inbox before returning to software; never overwrite late pending bits.
    pub fn put_file(&mut self, cpu: usize, file: File) {
        self.files.with_mut(|bank| bank.put_file(cpu, file));
    }
    pub fn source_pending_for(&self, cpu: usize, source: u32) -> bool {
        self.files.with(|bank| {
            self.aplic
                .source_eligible(cpu, source as usize, &bank.files[..self.cpus])
        })
    }
    pub fn retire_source(&mut self, source: u32) {
        self.aplic.retire_source(source);
    }
    pub fn rearms(&self) -> u64 {
        self.aplic.rearms
    }
    pub fn msis(&self) -> u64 {
        self.aplic.msis
    }

    /// All vCPUs have joined and producers are stopped. Discard all file/route
    /// state while retaining terminal diagnostics; cold reload uses new().
    pub fn quiesce(&mut self) {
        self.files.with_mut(Bank::quiesce);
        let (msis, rearms) = (self.aplic.msis, self.aplic.rearms);
        self.aplic = Aplic::new();
        self.aplic.msis = msis;
        self.aplic.rearms = rearms;
    }
    /// Validated aligned 32-bit access inside this VM's assigned 16 KiB region.
    pub fn aplic_access(&mut self, offset: usize, write: Option<usize>) -> Option<usize> {
        if offset >= APLIC_SIZE || offset & 3 != 0 {
            return None;
        }
        if let Some(value) = write {
            self.files.with_mut(|bank| {
                self.aplic
                    .write(offset, value as u32, &mut bank.files[..self.cpus])
            });
            Some(0)
        } else {
            Some(self.aplic.read(offset) as usize)
        }
    }
    /// An MSI port is not RAM: only naturally aligned 32-bit LE/BE stores at
    /// offsets 0/4 inject. Out-of-range IDs are ignored by the file, per AIA.
    pub fn msi_access(&mut self, offset: usize, write: Option<usize>) -> Option<usize> {
        let cpu = offset / MSI_FILE_SIZE;
        let port = offset % MSI_FILE_SIZE;
        if cpu >= self.cpus || !matches!(port, 0 | 4) {
            return None;
        }
        if let Some(value) = write {
            let id = if port == 0 {
                value as u32
            } else {
                (value as u32).swap_bytes()
            };
            self.files
                .with_mut(|bank| bank.files[cpu].inject(id as usize));
        }
        Some(0)
    }
    /// Return the old CSR value and any claimed wired-source mask. A CSR write
    /// to TOPEI claims once; reads, including masked-delivery reads, do not.
    pub fn csr(&mut self, cpu: usize, access: CsrAccess) -> Option<(usize, u32)> {
        // Only priority selectors still trap with a valid VGEIN. Do not use
        // the RAM inbox as a second, divergent EIP/EIE/TOPEI register bank.
        let value = self.files.with_mut(|bank| {
            if bank.hardware.bound & (1 << cpu) != 0
                && !(access.register == SIREG
                    && (PRIORITY_FIRST..=PRIORITY_LAST_EVEN).contains(&access.select)
                    && access.select & 1 == 0)
            {
                return None;
            }
            let value = match access.register {
                SIREG
                    if (PRIORITY_FIRST..=PRIORITY_LAST).contains(&access.select)
                        && access.select & 1 == 0 =>
                {
                    0
                }
                SIREG => bank.files[cpu].indirect(
                    access.select,
                    access.clear,
                    access.set,
                    access.write,
                )?,
                STOPEI => bank.files[cpu].topei(access.write),
                _ => return None,
            };
            Some(value)
        })?;
        let mut claimed = 0;
        if access.register == STOPEI && access.write && value != 0 {
            self.claims += 1;
            claimed = self.aplic.sources(cpu, value & 0x7ff);
        }
        self.csr_accesses += 1;
        Some((value, claimed))
    }
}
