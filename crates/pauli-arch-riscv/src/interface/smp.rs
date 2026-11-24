//! Native topology and once-started worker stacks, independent of guest harts.
//! Ownership/Lock Ordering: boot publishes topology/records before SBI HSM start.
//! Invariants: logical IDs map explicitly to physical firmware IDs; stacks never move.
use crate::host::{
    self,
    sbi::{self, SbiRet},
    smp::HartBoot,
};
use crate::native_harts::NativeHarts;
pub use crate::native_harts::worker_stack_bytes;
use pauli_core::{
    HvError, Result,
    arch::{ArchSmp, MAX_CPUS},
};
use pauli_hal::sync::{LocalIrq, Once};
struct Topology {
    harts: NativeHarts,
    #[cfg(feature = "aia-guest-file")]
    guest_ids: Option<usize>,
}
static TOPOLOGY: Once<Topology> = Once::new();
struct Start {
    entry: extern "C" fn(usize) -> !,
    opaque: usize,
}
static STARTS: [Once<Start>; MAX_CPUS] = [const { Once::new() }; MAX_CPUS];
static BOOTS: [Once<HartBoot>; MAX_CPUS] = [const { Once::new() }; MAX_CPUS];

/// A single-hart board needs no worker stack allocation.
pub fn install_topology(harts: &[usize], guest_ids: Option<usize>) -> Result<()> {
    install_with_stacks(harts, guest_ids, &mut [])
}
/// Transfer exclusive host memory to the workers before publishing any HSM record.
pub fn install_with_stacks(
    harts: &[usize],
    _guest_ids: Option<usize>,
    stacks: &'static mut [u8],
) -> Result<()> {
    let harts = NativeHarts::new(harts, host::cpu_id(), stacks)?;
    TOPOLOGY
        .set(Topology {
            harts,
            #[cfg(feature = "aia-guest-file")]
            guest_ids: _guest_ids,
        })
        .map_err(|_| HvError::Busy)
}
pub fn kick_cpu(owner: usize) -> SbiRet {
    let Some(hart) = TOPOLOGY.get().and_then(|t| t.harts.hart(owner)) else {
        return SbiRet {
            error: -3,
            value: 0,
        };
    };
    if owner == host::cpu_id() {
        // SAFETY: HS owns its software kick; release normal-memory publications first.
        unsafe {
            core::arch::asm!("fence w, o", "csrs sip, {ssip}", ssip=in(reg) crate::bits::SSIP, options(nostack));
        }
        SbiRet { error: 0, value: 0 }
    } else {
        sbi::send_ipi(hart)
    }
}
pub struct RiscvSmp;
impl ArchSmp for RiscvSmp {
    #[cfg(feature = "selftest")]
    const SAMPLE_IDLE: Option<fn() -> pauli_core::arch::IdleWake> = Some(crate::idle_wake::sample);
    #[cfg(feature = "selftest")]
    fn ack_sequence(owner: usize) -> u64 {
        crate::native_ack::RECEIPTS.sequence(owner)
    }
    #[cfg(feature = "selftest")]
    fn local_ack() -> Option<pauli_core::arch::NativeAck> {
        crate::native_ack::RECEIPTS.local(host::cpu_id())
    }
    fn cpu_id() -> usize {
        host::cpu_id()
    }
    fn kick(owner: usize) -> Result<()> {
        if kick_cpu(owner).error == 0 {
            Ok(())
        } else {
            Err(HvError::Fault)
        }
    }
    unsafe fn start(owner: usize, entry: extern "C" fn(usize) -> !, opaque: usize) -> Result<()> {
        let topology = TOPOLOGY.get().ok_or(HvError::InvalidState)?;
        let top = topology
            .harts
            .stack_top(owner)
            .ok_or(HvError::InvalidConfig)?;
        let hart = topology.harts.hart(owner).ok_or(HvError::InvalidConfig)?;
        STARTS[owner]
            .set(Start { entry, opaque })
            .map_err(|_| HvError::Busy)?;
        // SAFETY: this once-started native CPU exclusively owns a retained aligned stack.
        let boot = unsafe { HartBoot::new(top, trampoline, owner, owner) }.map_err(super::error)?;
        BOOTS[owner].set(boot).map_err(|_| HvError::Busy)?;
        let record = BOOTS[owner].get().ok_or(HvError::InvalidState)?;
        // SAFETY: boot/stack/runtime data were published and remain static until shutdown.
        let ret = unsafe { record.start(hart) };
        if ret.error == 0 {
            Ok(())
        } else {
            Err(HvError::Fault)
        }
    }
    unsafe fn idle() {
        // SAFETY: core put all banks, armed/rechecked wakeups and holds no lock.
        unsafe {
            host::irq::wait();
        }
    }
    fn acknowledge_kick() {
        let saved = host::irq::RiscvIrq::save();
        // SAFETY: same native owner, IRQs masked, bank already put before mailbox scan.
        unsafe {
            crate::remote::acknowledge_kick();
            host::irq::RiscvIrq::restore(saved);
        }
    }
}
extern "C" fn trampoline(_hart: usize, owner: usize) -> ! {
    // SAFETY: HSM assembly installed this CPU's retained stack and explicit tp.
    unsafe {
        host::trap::install_vector();
    }
    #[cfg(feature = "aia-guest-file")]
    if let Some(topology) = TOPOLOGY.get() {
        // SAFETY: board reserved file 1; before jobs, on this IRQ-masked HSM owner.
        unsafe {
            crate::aia::hardware::init_owner(topology.guest_ids);
        }
    }
    host::irq::enable_wake_sources(false);
    // SAFETY: shared native IRQ dispatcher was installed before starting workers.
    unsafe {
        host::irq::enable();
    }
    match STARTS[owner].get() {
        Some(start) => (start.entry)(start.opaque),
        // Internal publication fault only: no guest input selects a worker record.
        None => panic!("native worker before boot-record publication"),
    }
}
