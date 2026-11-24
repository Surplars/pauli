//! Declarative fixed-capacity MMIO routing; devices stay with their VM owner.
//! Ownership/Lock Ordering: immutable routes select one device before its lock.
//! Invariants: no overlapping/overflowing regions or accesses crossing a region.
use crate::{HvError, Result, arch::IrqRegion};
use pauli_config::MmioRegion;
use pauli_hal::device::MmioAccess;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceId {
    Uart,
    Block,
    Net,
    Rtc,
    Irq(IrqRegion),
}
#[derive(Clone, Copy)]
struct Route {
    base: usize,
    end: usize,
    device: DeviceId,
}
pub struct MmioBus<const N: usize = 8> {
    routes: [Option<Route>; N],
    used: usize,
}
pub trait MmioDevice {
    fn access(&mut self, offset: usize, width: usize, write: Option<usize>) -> Option<usize>;
}
impl<const N: usize> MmioBus<N> {
    pub const fn new() -> Self {
        Self {
            routes: [None; N],
            used: 0,
        }
    }
    pub fn register(&mut self, region: MmioRegion, device: DeviceId) -> Result<()> {
        let base = usize::try_from(region.base).map_err(|_| HvError::InvalidDevice)?;
        let size = usize::try_from(region.size).map_err(|_| HvError::InvalidDevice)?;
        let end = base
            .checked_add(size)
            .filter(|_| size != 0)
            .ok_or(HvError::InvalidDevice)?;
        if self.routes[..self.used]
            .iter()
            .flatten()
            .any(|r| base < r.end && r.base < end)
        {
            return Err(HvError::InvalidDevice);
        }
        let slot = self.routes.get_mut(self.used).ok_or(HvError::Exhausted)?;
        *slot = Some(Route { base, end, device });
        self.used += 1;
        Ok(())
    }
    pub fn resolve(&self, access: MmioAccess) -> Option<(DeviceId, usize)> {
        if !matches!(access.width, 1 | 2 | 4 | 8) || access.gpa & (access.width - 1) != 0 {
            return None;
        }
        let end = access.gpa.checked_add(access.width)?;
        self.routes[..self.used]
            .iter()
            .flatten()
            .find(|route| access.gpa >= route.base && end <= route.end)
            .map(|route| (route.device, access.gpa - route.base))
    }
    pub fn validate_ram(&self, base: usize, size: usize) -> Result<()> {
        let end = base.checked_add(size).ok_or(HvError::InvalidMemory)?;
        if self.routes[..self.used]
            .iter()
            .flatten()
            .any(|route| base < route.end && route.base < end)
        {
            return Err(HvError::InvalidMemory);
        }
        Ok(())
    }
}
