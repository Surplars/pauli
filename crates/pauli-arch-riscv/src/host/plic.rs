//! Native PLIC protocol with board-resolved supervisor contexts.
//! Ownership/Lock Ordering: short IRQ-safe route lock for enable-word RMW; claim is hardware-atomic.
//! Invariants: no physical base/context stride policy is guessed from a hart number.

use super::irq::RiscvIrq;
use pauli_hal::{
    device::{DeviceError, DeviceResource, Result},
    irq::HostIrqController,
    mmio::Mmio,
    sync::IrqSpinLock,
};
const ENABLE: usize = 0x2000;
const CONTEXT: usize = 0x20_0000;
const ENABLE_STRIDE: usize = 0x80;
const CONTEXT_STRIDE: usize = 0x1000;
const CLAIM: usize = 4;
const MAX_SOURCE: u32 = 1023;

pub struct Plic {
    mmio: Mmio,
    contexts: [usize; crate::MAX_CPUS],
    cpus: usize,
    sources: u32,
    routes: IrqSpinLock<(), RiscvIrq>,
}
impl Plic {
    /// # Safety
    /// Native mapped PLIC, with exclusive supervisor context/source routing
    /// ownership resolved from firmware's interrupt-controller topology.
    pub unsafe fn new(
        resource: DeviceResource,
        contexts: [usize; crate::MAX_CPUS],
        cpus: usize,
        sources: u32,
    ) -> Result<Self> {
        if cpus == 0
            || cpus > crate::MAX_CPUS
            || sources == 0
            || sources > MAX_SOURCE
            || resource.base & 3 != 0
        {
            return Err(DeviceError::InvalidResource);
        }
        let mut minimum = 0;
        for (index, context) in contexts[..cpus].iter().enumerate() {
            if contexts[..index].contains(context) {
                return Err(DeviceError::InvalidResource);
            }
            minimum = minimum.max(
                context
                    .checked_mul(CONTEXT_STRIDE)
                    .and_then(|offset| offset.checked_add(CONTEXT + CLAIM + 4))
                    .ok_or(DeviceError::InvalidResource)?,
            );
        }
        // SAFETY: caller supplied persistent native device memory; all context windows fit.
        let mmio = unsafe { Mmio::new(resource, minimum)? };
        for context in &contexts[..cpus] {
            mmio.write32(CONTEXT + context * CONTEXT_STRIDE, 0)?;
        }
        Ok(Self {
            mmio,
            contexts,
            cpus,
            sources,
            routes: IrqSpinLock::new(()),
        })
    }
    fn context(&self, cpu: usize) -> Result<usize> {
        self.contexts
            .get(cpu)
            .copied()
            .filter(|_| cpu < self.cpus)
            .ok_or(DeviceError::InvalidResource)
    }
    fn route(&self, cpu: usize, source: u32, enabled: bool) -> Result<()> {
        let context = self.context(cpu)?;
        if source == 0 || source > self.sources {
            return Err(DeviceError::InvalidResource);
        }
        let _owner = self.routes.lock();
        if enabled {
            self.mmio.write32(source as usize * 4, 1)?;
        }
        let offset = ENABLE + context * ENABLE_STRIDE + source as usize / 32 * 4;
        let before = self.mmio.read32(offset)?;
        let bit = 1 << (source % 32);
        self.mmio
            .write32(offset, if enabled { before | bit } else { before & !bit })
    }
}
impl HostIrqController for Plic {
    fn enable(&self, cpu: usize, irq: u32) -> Result<()> {
        self.route(cpu, irq, true)
    }
    fn disable(&self, cpu: usize, irq: u32) -> Result<()> {
        self.route(cpu, irq, false)
    }
    fn claim(&self, cpu: usize) -> Option<u32> {
        let context = self.context(cpu).ok()?;
        self.mmio
            .read32(CONTEXT + context * CONTEXT_STRIDE + CLAIM)
            .ok()
            .filter(|irq| *irq != 0)
    }
    fn complete(&self, cpu: usize, irq: u32) {
        if let Ok(context) = self.context(cpu) {
            let _ = self
                .mmio
                .write32(CONTEXT + context * CONTEXT_STRIDE + CLAIM, irq);
        }
    }
}
