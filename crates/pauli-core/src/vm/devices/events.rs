//! Self-only control events share the inbox lock with enqueue, dequeue and revocation.
use super::{IoState, VirtualIrqChip, VmDevices};
use pauli_control::abi::{
    EVENT_LOG, EVENT_MAIL, EVENT_RESCAN, EVENT_STATE, INVALID_STATE, NOT_SUPPORTED,
};

const OBSERVED: [u64; 2] = [EVENT_LOG, EVENT_STATE];

// Epochs are unique across VMs. Remember enough provenance to withdraw a
// revoked source without an unbounded list of peers on the vCPU's TAKE path.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Origins {
    #[default]
    Empty,
    One(u32),
    Several,
}

#[derive(Default)]
pub(super) struct ControlEvents {
    enabled: u64,
    rescan_for: u64,
    observed: [Origins; 2],
    asserted: bool,
}

impl IoState {
    fn pending_classes(&self) -> u64 {
        let mut bits = u64::from(self.mailbox.has_hint()) * EVENT_MAIL;
        for (class, origin) in OBSERVED.into_iter().zip(self.control.observed) {
            if origin != Origins::Empty {
                bits |= class;
            }
        }
        bits
    }
}

impl<I: VirtualIrqChip> VmDevices<I> {
    pub(crate) fn refresh_control_event(&self, io: &mut IoState) {
        self.drive_control_irq(io, 0);
    }

    /// The manager has checked the receiver's rights on this peer.
    pub(crate) fn publish_control_event(&self, io: &mut IoState, peer_epoch: u32, classes: u64) {
        for (class, origin) in OBSERVED.into_iter().zip(&mut io.control.observed) {
            if class & classes != 0 {
                *origin = match *origin {
                    Origins::Empty => Origins::One(peer_epoch),
                    Origins::One(old) if old == peer_epoch => Origins::One(old),
                    _ => Origins::Several,
                };
            }
        }
        self.refresh_control_event(io);
    }

    pub(crate) fn invalidate_control_event(&self, io: &mut IoState, peer_epoch: u32, classes: u64) {
        io.control.rescan_for |= classes & io.control.enabled;
        for (class, origin) in OBSERVED.into_iter().zip(&mut io.control.observed) {
            if classes & class == 0 {
                continue;
            }
            match *origin {
                Origins::Several => {
                    // Other peers may still have pending work, even if this
                    // class is masked now. Preserve its wake on a later unmask.
                    io.control.rescan_for |= class;
                    *origin = Origins::Empty;
                }
                Origins::One(old) if old == peer_epoch => *origin = Origins::Empty,
                _ => {}
            }
        }
        self.refresh_control_event(io);
    }

    fn drive_control_irq(&self, io: &mut IoState, exclude: usize) {
        let Some(source) = self.control_irq else {
            return;
        };
        let asserted = io.control.enabled != 0
            && io.control.enabled & (io.pending_classes() | io.control.rescan_for) != 0;
        if asserted != io.control.asserted {
            self.change_irqs(exclude, |chip| chip.set_level(source, asserted));
            io.control.asserted = asserted;
        }
    }
}

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
                let reasons = io.pending_classes();
                io.mailbox.clear_hints();
                io.control.observed = [Origins::Empty; 2];
                let rescan = core::mem::take(&mut io.control.rescan_for) != 0;
                reasons | (u64::from(rescan) * EVENT_RESCAN)
            }
            EventControl::Mask(enabled) => core::mem::replace(&mut io.control.enabled, enabled),
        };
        // The issuing lane will resample at guest entry; changes for other
        // recipients use the ordinary wake bitmap and native kick retry path.
        self.devices.drive_control_irq(&mut io, 1 << issuer);
        Ok(value)
    }
}
