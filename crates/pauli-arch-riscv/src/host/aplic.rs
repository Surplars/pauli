//! Native delegated S-APLIC routing; parent MSI address configuration stays in firmware.
//! Ownership/Lock Ordering: board serializes route programming; native claim/complete is local.
//! Invariants: source ownership/readback is verified; no M-domain or guest file is programmed.

use pauli_hal::{
    device::{DeviceError, DeviceResource, Result},
    mmio::Mmio,
};
const DOMAINCFG: usize = 0;
const SOURCECFG: usize = 4;
const SETIENUM: usize = 0x1edc;
const CLRIENUM: usize = 0x1fdc;
const SETIPNUM: usize = 0x2000;
const TARGET: usize = 0x3004;
const IDC: usize = 0x4000;
const IDC_STRIDE: usize = 0x20;
const DELIVERY: usize = 0;
const THRESHOLD: usize = 8;
const CLAIM: usize = 0x1c;
const DOMAIN_ID: u32 = 0x8000_0000;
const DOMAIN_ID_MASK: u32 = 0xff00_0000;
const DOMAIN_IE: u32 = 1 << 8;
const DOMAIN_MSI: u32 = 1 << 2;
const DOMAIN_BE: u32 = 1;
const HART_SHIFT: u32 = 18;
const ID_MASK: u32 = 0x7ff;
pub const SOURCE_LEVEL_HIGH: u32 = 6;
pub const SOURCE_EDGE_RISING: u32 = 4;
pub struct Aplic {
    mmio: Mmio,
    sources: u32,
    msi: bool,
}
impl Aplic {
    /// # Safety
    /// Persistent native S-domain mapping exclusively delegated to this host.
    /// Firmware must have configured root MSI addresses if msi=true.
    pub unsafe fn new(resource: DeviceResource, sources: u32, msi: bool) -> Result<Self> {
        if sources == 0 || sources > 1023 || resource.base & 3 != 0 {
            return Err(DeviceError::InvalidResource);
        }
        // Only the discovered direct context is required, not MAX_CPUS IDC windows.
        // SAFETY: caller supplies a native delegated domain; init_direct checks its IDC.
        let mmio = unsafe { Mmio::new(resource, if msi { IDC } else { IDC + IDC_STRIDE })? };
        let before = mmio.read32(DOMAINCFG)?;
        if before & DOMAIN_ID_MASK != DOMAIN_ID || before & DOMAIN_BE != 0 {
            return Err(DeviceError::NotSupported);
        }
        let mode = DOMAIN_IE | if msi { DOMAIN_MSI } else { 0 };
        mmio.write32(DOMAINCFG, mode)?;
        if mmio.read32(DOMAINCFG)? & (DOMAIN_ID_MASK | DOMAIN_IE | DOMAIN_MSI | DOMAIN_BE)
            != DOMAIN_ID | mode
        {
            return Err(DeviceError::NotSupported);
        }
        Ok(Self { mmio, sources, msi })
    }
    pub fn route(&self, source: u32, hart_index: usize, identity: u32, mode: u32) -> Result<()> {
        if source == 0
            || source > self.sources
            || hart_index >= (1 << 14)
            || identity == 0
            || identity > ID_MASK
            || !matches!(mode, SOURCE_LEVEL_HIGH | SOURCE_EDGE_RISING)
        {
            return Err(DeviceError::InvalidResource);
        }
        self.mmio
            .write32(SOURCECFG + (source as usize - 1) * 4, mode)?;
        if self.mmio.read32(SOURCECFG + (source as usize - 1) * 4)? != mode {
            return Err(DeviceError::NotSupported);
        }
        self.mmio.write32(
            TARGET + (source as usize - 1) * 4,
            ((hart_index as u32) << HART_SHIFT) | identity,
        )
    }
    pub fn set_enabled(&self, source: u32, enabled: bool) -> Result<()> {
        if source == 0 || source > self.sources {
            return Err(DeviceError::InvalidResource);
        }
        io_fence();
        self.mmio
            .write32(if enabled { SETIENUM } else { CLRIENUM }, source)?;
        io_fence();
        Ok(())
    }
    pub fn init_direct(&self, context: usize) -> Result<()> {
        if self.msi || context >= (self.mmio.size() - IDC) / IDC_STRIDE {
            return Err(DeviceError::InvalidResource);
        }
        let base = IDC + context * IDC_STRIDE;
        self.mmio.write32(base + THRESHOLD, 0)?;
        self.mmio.write32(base + DELIVERY, 1)
    }
    pub fn claim_direct(&self, context: usize) -> Option<u32> {
        if self.msi {
            return None;
        }
        self.mmio
            .read32(
                IDC.checked_add(context.checked_mul(IDC_STRIDE)?)?
                    .checked_add(CLAIM)?,
            )
            .ok()
            .map(|value| value >> 16)
            .filter(|id| *id != 0)
    }
    pub fn rearm_level(&self, source: u32) -> Result<()> {
        if source == 0 || source > self.sources {
            return Err(DeviceError::InvalidResource);
        }
        io_fence();
        self.mmio.write32(SETIPNUM, source)?;
        io_fence();
        Ok(())
    }
}
fn io_fence() {
    // SAFETY: native I/O ordering only; no addresses or guest state are accessed.
    unsafe {
        core::arch::asm!("fence iorw, iorw", options(nostack));
    }
}
