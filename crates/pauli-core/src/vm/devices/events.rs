//! Self-only control events share the inbox lock with enqueue, dequeue and revocation.
use super::{IoState, VirtualIrqChip, VmDevices};
#[cfg(feature = "guest-control")]
use pauli_control::abi::{EVENT_MAIL, EVENT_RESCAN, INVALID_STATE, NOT_SUPPORTED};

#[derive(Default)]
pub(super) struct ControlEvents {
    enabled: bool,
    rescan: bool,
    asserted: bool,
}

impl<I: VirtualIrqChip> VmDevices<I> {
    pub(crate) fn refresh_control_event(&self, io: &mut IoState) {
        self.drive_control_irq(io, 0);
    }

    /// A previous MAIL binding became unobservable. A subscribed waiter must
    /// recheck its grants even when all of that peer's hints have been removed.
    pub(crate) fn invalidate_control_event(&self, io: &mut IoState) {
        if io.control.enabled {
            io.control.rescan = true;
        }
        self.refresh_control_event(io);
    }

    fn drive_control_irq(&self, io: &mut IoState, exclude: usize) {
        let Some(source) = self.control_irq else {
            return;
        };
        let asserted = io.control.enabled && (io.control.rescan || io.mailbox.has_hint());
        if asserted != io.control.asserted {
            self.change_irqs(exclude, |chip| chip.set_level(source, asserted));
            io.control.asserted = asserted;
        }
    }
}

#[cfg(feature = "guest-control")]
impl<A: crate::arch::HvArch> crate::vm::Vm<A> {
    pub(crate) fn control_event(
        &self,
        control: crate::arch::EventControl,
        issuer: u8,
    ) -> Result<u64, i64> {
        use crate::arch::EventControl;
        self.devices.control_irq.ok_or(NOT_SUPPORTED)?;
        let mut io = self.devices.io.lock();
        // An admitted vCPU pins its epoch. A native stop may still close it
        // while this exit waits for the receiver's I/O lock.
        if !self.lifecycle.can_enter() {
            return Err(INVALID_STATE);
        }
        let value = match control {
            EventControl::Take => {
                let mail = io.mailbox.has_hint();
                io.mailbox.clear_hints();
                let rescan = core::mem::take(&mut io.control.rescan);
                (u64::from(mail) * EVENT_MAIL) | (u64::from(rescan) * EVENT_RESCAN)
            }
            EventControl::Mask(enabled) => {
                u64::from(core::mem::replace(&mut io.control.enabled, enabled)) * EVENT_MAIL
            }
        };
        // The issuing lane will resample at guest entry; changes for other
        // recipients use the ordinary wake bitmap and native kick retry path.
        self.devices.drive_control_irq(&mut io, 1 << issuer);
        Ok(value)
    }
}
