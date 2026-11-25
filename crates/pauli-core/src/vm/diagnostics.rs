//! VM-local failure records. Only an unproven device join is permanently latched.
use super::Vm;
use crate::{
    HvError,
    arch::{ArchSmp, HvArch, SmpError},
};
use core::sync::atomic::{AtomicU8, Ordering};
use pauli_hal::sync::{Once, SpinLock};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct KickFault {
    pub epoch: u32,
    pub owner: usize,
    pub source: SmpError,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActivationFault {
    Activation {
        epoch: u32,
        owner: usize,
        error: HvError,
    },
    StartKick(KickFault),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ManagementFault {
    pub epoch: u32,
    pub owner: usize,
    pub error: HvError,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct VmFaults {
    pub activation: Option<ActivationFault>,
    pub shutdown_kick: Option<KickFault>,
}

const ACTIVATION: u8 = 1;
const SHUTDOWN_KICK: u8 = 2;
const IO_JOIN: u8 = 4;
pub(super) struct Diagnostics {
    faults: SpinLock<VmFaults>,
    join: Once<ManagementFault>,
    pending: AtomicU8,
}

impl Diagnostics {
    pub(super) fn new() -> Self {
        Self {
            faults: SpinLock::new(VmFaults::default()),
            join: Once::new(),
            pending: AtomicU8::new(0),
        }
    }
}

impl<A: HvArch> Vm<A> {
    pub(crate) fn faults(&self) -> VmFaults {
        *self.diagnostics.faults.lock()
    }

    pub(crate) fn join_fault(&self) -> Option<ManagementFault> {
        self.diagnostics.join.get().copied()
    }

    pub fn join_error(&self) -> Option<HvError> {
        self.join_fault().map(|fault| fault.error)
    }

    pub(super) fn begin_activation(&self) {
        // Rebuild owns the joined VM. All outgoing owners and kicks have retired.
        *self.diagnostics.faults.lock() = VmFaults::default();
        self.diagnostics
            .pending
            .fetch_and(IO_JOIN, Ordering::Relaxed);
    }

    pub(super) fn record_activation(&self, fault: ActivationFault) {
        self.diagnostics.faults.lock().activation = Some(fault);
        self.diagnostics
            .pending
            .fetch_or(ACTIVATION, Ordering::Release);
    }

    pub(super) fn record_shutdown_kick(&self, fault: KickFault) {
        let mut faults = self.diagnostics.faults.lock();
        if faults.shutdown_kick.is_none() {
            faults.shutdown_kick = Some(fault);
            self.diagnostics
                .pending
                .fetch_or(SHUTDOWN_KICK, Ordering::Release);
        }
    }

    pub(crate) fn fail_join(&self, error: HvError) {
        let fault = ManagementFault {
            epoch: self.epoch(),
            owner: A::Smp::cpu_id(),
            error,
        };
        if self.diagnostics.join.set(fault).is_ok() {
            self.diagnostics
                .pending
                .fetch_or(IO_JOIN, Ordering::Release);
            self.request_stop(true, false);
        }
    }

    /// Only the management monitor consumes notices. An unchanged poll takes no lock.
    pub(crate) fn take_fault_notice(&self) -> Option<(VmFaults, Option<ManagementFault>)> {
        if self.diagnostics.pending.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let pending = self.diagnostics.pending.swap(0, Ordering::AcqRel);
        let faults = self.faults();
        Some((
            VmFaults {
                activation: faults.activation.filter(|_| pending & ACTIVATION != 0),
                shutdown_kick: faults
                    .shutdown_kick
                    .filter(|_| pending & SHUTDOWN_KICK != 0),
            },
            self.join_fault().filter(|_| pending & IO_JOIN != 0),
        ))
    }
}
