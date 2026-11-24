//! Host interrupt controller abstraction and bounded boot-registered routes.
//! Ownership/Lock Ordering: board owns routing; IRQ dispatch takes no registry lock.
//! Invariants: route IDs are unique; handlers run only after registration is frozen.

use crate::device::{DeviceError, Result};

pub trait HostIrqController: Send + Sync {
    fn enable(&self, cpu: usize, irq: u32) -> Result<()>;
    fn disable(&self, cpu: usize, irq: u32) -> Result<()>;
    fn claim(&self, cpu: usize) -> Option<u32>;
    fn complete(&self, cpu: usize, irq: u32);
}
pub trait IrqHandler: Send + Sync {
    fn handle_irq(&self) -> bool;
}

pub struct IrqRoutes<const N: usize> {
    routes: [Option<(u32, &'static dyn IrqHandler)>; N],
}
impl<const N: usize> IrqRoutes<N> {
    pub const fn new() -> Self {
        Self { routes: [None; N] }
    }
    pub fn register(&mut self, irq: u32, handler: &'static dyn IrqHandler) -> Result<()> {
        if irq == 0 || self.routes.iter().flatten().any(|(id, _)| *id == irq) {
            return Err(DeviceError::InvalidResource);
        }
        let slot = self
            .routes
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(DeviceError::Exhausted)?;
        *slot = Some((irq, handler));
        Ok(())
    }
    pub fn dispatch(&self, irq: u32) -> bool {
        self.routes
            .iter()
            .flatten()
            .find(|(id, _)| *id == irq)
            .is_some_and(|(_, handler)| handler.handle_irq())
    }
}
impl<const N: usize> Default for IrqRoutes<N> {
    fn default() -> Self {
        Self::new()
    }
}
