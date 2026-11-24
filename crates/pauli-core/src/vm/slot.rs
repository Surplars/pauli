//! One pinned vCPU's short metadata lock and separately locked mailbox.
//! Ownership/Lock Ordering: take CPU under metadata lock, release before every run.
//! Invariants: context is absent only while its sole native owner holds it by value.
//! CPU state: Stopped -> Starting -> Runnable <-> Waiting/Fence -> Stopped.
use crate::{
    HvError, Result,
    arch::{self, ArchVcpu, MAX_VCPUS, Mailbox},
};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use pauli_hal::sync::SpinLock;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuState {
    Stopped,
    Starting,
    Stopping,
    Runnable,
    Waiting,
    Blocked,
}
impl CpuState {
    /// A started hart remains started to its SBI caller while parked by the scheduler.
    pub(crate) fn hsm_status(self) -> arch::CpuStatus {
        match self {
            Self::Stopped => arch::CpuStatus::Stopped,
            Self::Starting => arch::CpuStatus::StartPending,
            Self::Stopping => arch::CpuStatus::StopPending,
            Self::Runnable | Self::Waiting | Self::Blocked => arch::CpuStatus::Started,
        }
    }
}
pub type Ticket<V> = <<V as ArchVcpu>::Mailbox as Mailbox<V>>::Ticket;
pub struct FenceWait<V: ArchVcpu> {
    pub tickets: [Option<Ticket<V>>; MAX_VCPUS],
}
pub enum Pending<V: ArchVcpu> {
    Fence(FenceWait<V>),
    #[cfg(feature = "guest-control")]
    Management(pauli_control::queue::Ticket),
}
pub struct CpuData<V: ArchVcpu> {
    pub context: Option<V>,
    pub state: CpuState,
    pub start: Option<(usize, usize)>,
    pub pending: Option<Pending<V>>,
    /// Native ticks of a previously cut quantum still protected on this lane.
    pub(crate) protected_ticks: u64,
}
pub struct VcpuSlot<V: ArchVcpu> {
    pub owner: usize,
    pub index: u8,
    pub data: SpinLock<CpuData<V>>,
    pub mailbox: SpinLock<V::Mailbox>,
    // Advisory only. The owner publishes the *enabled* compare on returning
    // a WFI context; each new incarnation starts with no parked timer.
    pub(crate) parked_timer: AtomicU64,
    retired: AtomicBool,
}
impl<V: ArchVcpu> VcpuSlot<V> {
    pub fn new(context: V, epoch: u32, index: u8, owner: usize) -> Self {
        Self {
            owner,
            index,
            data: SpinLock::new(CpuData {
                context: Some(context),
                state: if index == 0 {
                    CpuState::Runnable
                } else {
                    CpuState::Stopped
                },
                start: None,
                pending: None,
                protected_ticks: 0,
            }),
            mailbox: SpinLock::new(V::Mailbox::new(epoch, index, owner)),
            parked_timer: AtomicU64::new(u64::MAX),
            retired: AtomicBool::new(true),
        }
    }
    pub fn retired(&self) -> bool {
        self.retired.load(Ordering::Acquire)
    }
    pub fn entered(&self) {
        self.retired.store(false, Ordering::Release);
    }
    pub fn acknowledge_retired(&self, cpu: &V) {
        self.retired
            .store(cpu.translations_retired(), Ordering::Release);
    }
    pub fn request_start(&self, entry: usize, opaque: usize) -> Result<()> {
        let mut data = self.data.lock();
        if data.state != CpuState::Stopped {
            return Err(HvError::Busy);
        }
        data.start = Some((entry, opaque));
        data.state = CpuState::Starting;
        Ok(())
    }
    /// The owner's last published scheduler state, copied before monitor output.
    pub(crate) fn run_state(&self) -> CpuState {
        self.data.lock().state
    }
    pub fn status(&self) -> arch::CpuStatus {
        self.run_state().hsm_status()
    }
    /// Called only with the entire VM frozen in Joined state.
    pub fn replace_joined(&self, cpu: V, epoch: u32) -> Result<()> {
        let mut data = self.data.lock();
        if data.context.is_none() || !self.retired() {
            return Err(HvError::Busy);
        }
        *data = CpuData {
            context: Some(cpu),
            state: if self.index == 0 {
                CpuState::Runnable
            } else {
                CpuState::Stopped
            },
            start: None,
            pending: None,
            protected_ticks: 0,
        };
        self.parked_timer.store(u64::MAX, Ordering::Relaxed);
        *self.mailbox.lock() = V::Mailbox::new(epoch, self.index, self.owner);
        Ok(())
    }
}
