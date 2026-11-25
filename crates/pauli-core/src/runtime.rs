//! One pinned-owner dispatcher for both single-CPU and SMP configurations.
//! Ownership/Lock Ordering: take context briefly, drop locks, run, restore metadata.
//! Invariants: same absolute slice budget across quick exits; put before native control.
//! Owner: scan mailboxes -> select -> run/park -> put -> return context -> idle.
use crate::{
    HvError, Result,
    arch::{self, ArchSmp, ArchTimer, ArchVcpu, HvArch, Mailbox, Resident},
    board::Board,
    manager::{VmManager, VmStore},
    placement::{self, Placement},
    trace::{NoopTracer, Tracer},
    vm::{
        State, Vm,
        slot::{CpuState, FenceWait, Pending, VcpuSlot},
    },
};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use pauli_hal::sync::LocalIrq;
mod control;
mod deadline;
mod dispatch;
mod idle;
#[cfg(feature = "selftest")]
mod idle_observation;
mod owner;
mod pending;
mod workers;
pub use workers::WorkerStartupError;

pub struct VmRuntime<A: HvArch, B: Board> {
    pub board: &'static B,
    pub manager: VmManager<A>,
    firmware_only: bool,
    diskless_single_hart: bool,
    // Written and consumed only on the single management hart.
    service_due: AtomicBool,
    online_workers: AtomicU64,
    ready: AtomicBool,
    #[cfg(feature = "selftest")]
    idle_wakes: idle_observation::IdleCounters,
    #[cfg(feature = "guest-control")]
    pub(crate) rpc: pauli_hal::sync::SpinLock<pauli_control::queue::Queue>,
}
pub struct Dispatcher<T: ArchTimer, R: Tracer = NoopTracer> {
    timer: T,
    cursor: usize,
    timer_turn: deadline::TimerTurn,
    tracer: R,
}
impl<T: ArchTimer> Dispatcher<T> {
    pub fn new() -> Self {
        Self::with_tracer(NoopTracer)
    }
}
impl<T: ArchTimer, R: Tracer> Dispatcher<T, R> {
    pub fn with_tracer(tracer: R) -> Self {
        Self {
            timer: T::new(),
            cursor: 0,
            timer_turn: deadline::TimerTurn::Normal,
            tracer,
        }
    }
    pub fn tracer(&self) -> &R {
        &self.tracer
    }
}
impl<A: HvArch, B: Board> VmRuntime<A, B> {
    pub(crate) fn create(board: &'static B, storage: &'static VmStore<A>) -> Result<Self> {
        A::init(board.layout())?;
        let manager = VmManager::create(board, storage)?;
        // Checked once: a firmware template can only produce another single-vCPU,
        // diskless instance. A declared native disk may need service even unattached.
        let firmware_only = manager.config.disks.iter().all(Option::is_none)
            && manager
                .config
                .vms
                .iter()
                .flatten()
                .all(|vm| vm.firmware.is_some());
        let diskless_single_hart = board.info().host_cpus == 1
            && B::IRQ_DRIVEN_CONSOLE
            && manager.config.disks.iter().all(Option::is_none)
            && manager.config.vms.iter().flatten().all(|vm| {
                vm.firmware.is_none() && !vm.has_net() && vm.vcpus == 1
            });
        Ok(Self {
            board,
            manager,
            firmware_only,
            diskless_single_hart,
            service_due: AtomicBool::new(false),
            online_workers: AtomicU64::new(0),
            ready: AtomicBool::new(false),
            #[cfg(feature = "selftest")]
            idle_wakes: Default::default(),
            #[cfg(feature = "guest-control")]
            rpc: pauli_hal::sync::SpinLock::new(pauli_control::queue::Queue::new()),
        })
    }
}

fn masked<A: HvArch, T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    let saved = A::IrqMask::save();
    let result = f();
    // SAFETY: same native owner; the scoped operation returned all locks and banks.
    unsafe { A::IrqMask::restore(saved) };
    result
}
