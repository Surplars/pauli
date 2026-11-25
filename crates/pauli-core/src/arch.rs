//! ISA boundary: native ownership, second-stage maps and vCPU exits.
//! Ownership/Lock Ordering: owner holds no business lock across run or native idle.
//! Invariants: pinned owners; absolute deadlines; foreign control first puts banks.
use crate::{
    HvError, Result,
    board::Payload,
    memory::{Arena, GuestRam},
};
use pauli_config::{GuestBoardLayout, VmConfig};
use pauli_hal::{device::MmioAccess, sync::LocalIrq};

pub const MAX_CPUS: usize = pauli_config::MAX_HOST_CPUS as usize;
pub const MAX_VCPUS: usize = pauli_config::MAX_GUEST_VCPUS as usize;
const _: () = assert!(MAX_VCPUS <= usize::BITS as usize);
/// Validate every bit before an IPI/fence has side effects. The all-CPU
/// sentinel ignores the mask; an empty ordinary mask never addresses a CPU.
pub fn cpu_targets(mask: usize, base: usize, count: usize) -> Option<usize> {
    if !(1..=MAX_VCPUS).contains(&count) {
        return None;
    }
    let all = usize::MAX >> (usize::BITS as usize - count);
    if base == usize::MAX {
        return Some(all);
    }
    if mask == 0 {
        return Some(0);
    }
    if base >= count || mask & !(all >> base) != 0 {
        return None;
    }
    Some(mask << base)
}
// Transport backends announce the same ABI without importing the policy crate.
#[cfg(feature = "guest-control")]
pub use pauli_control::abi::EID as MANAGEMENT_EID;
/// # Safety
/// All arena pointers and RAM mappings must be confined to the resulting
/// space and its VM. No references may escape a failed build or outlive the
/// dropped space after all owners have retired their translations.
pub unsafe trait Stage2Space: Send + Sync {
    /// # Safety
    /// The caller exclusively assigns/retains RAM and private table arena;
    /// no table or RAM is live until construction and image publication finish.
    unsafe fn create(ram: &GuestRam, arena: &mut Arena, payload: Payload) -> Result<Self>
    where
        Self: Sized;
    fn executable(&self, address: usize) -> bool;
}
#[derive(Clone, Copy)]
pub struct VcpuBoot {
    pub entry: usize,
    pub opaque: usize,
    pub payload: Payload,
}
#[derive(Clone, Copy, Debug)]
pub struct RegisterAccess {
    pub register: u16,
    pub select: usize,
    pub clear: usize,
    pub set: usize,
    pub write: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqRegion {
    Legacy,
    Wired,
    Message,
}
/// Native interrupt-file transfers for this VM incarnation, not a capability flag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IrqFileStats {
    pub loaded_vcpus: usize,
    pub loads: u64,
    pub puts: u64,
    pub merges: u64,
    pub injected: u64,
    pub bound: usize,
}
pub trait VirtualIrqChip: Send {
    fn create(vcpus: u8, advanced: bool) -> Result<Self>
    where
        Self: Sized;
    /// Construct the same interrupt geometry the board advertised in its guest DTB.
    fn create_with_imsic_ids(vcpus: u8, advanced: bool, ids: pauli_config::ImsicIds) -> Result<Self>
    where
        Self: Sized,
    {
        if advanced && ids != pauli_config::ImsicIds::Ids255 {
            return Err(HvError::Unsupported);
        }
        Self::create(vcpus, advanced)
    }
    fn access(&mut self, region: IrqRegion, offset: usize, write: Option<usize>) -> Option<usize>;
    fn register(&mut self, cpu: u8, access: RegisterAccess) -> Option<usize>;
    fn set_level(&mut self, source: u32, asserted: bool);
    fn pending(&self, cpu: u8) -> bool;
    fn quiesce(&mut self) -> Result<()>;
    /// Optional control-plane diagnostic. Never called on the dispatch path.
    fn file_stats(&self) -> Option<IrqFileStats> {
        None
    }
}
pub trait GuestIo {
    fn mmio(&mut self, access: MmioAccess) -> Option<usize>;
    fn register(&mut self, cpu: u8, access: RegisterAccess) -> Option<usize>;
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Start {
        cpu: usize,
        entry: usize,
        opaque: usize,
    },
    StopCpu,
    CpuStatus {
        cpu: usize,
    },
    Ipi {
        mask: usize,
        base: usize,
    },
    Fence {
        mask: usize,
        base: usize,
        instruction: bool,
    },
    #[cfg(feature = "guest-control")]
    ManagementProbe,
    #[cfg(feature = "guest-control")]
    Management {
        args: [u64; 6],
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlError {
    Failed,
    Unsupported,
    InvalidParameter,
    InvalidAddress,
    AlreadyAvailable,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlReply {
    Success(usize),
    CpuStatus(CpuStatus),
    Error(ControlError),
    #[cfg(feature = "guest-control")]
    Transport {
        error: i64,
        value: u64,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuStatus {
    Started,
    Stopped,
    StartPending,
    StopPending,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitKind {
    Resume,
    Wait,
    Shutdown,
    Reset,
    Fault,
    Control(Control),
}
/// The decoded operation or native interrupt responsible for a guest exit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExitSource {
    Timer,
    External,
    Software,
    SupervisorCall,
    Mmio,
    Wfi,
    #[default]
    Other,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TickInterval {
    pub start: u64,
    pub end: u64,
}
impl TickInterval {
    /// Take samples immediately around the switch or WFI, only when requested.
    #[inline]
    pub fn measure<T>(out: Option<&mut Self>, now: impl Fn() -> u64, run: impl FnOnce() -> T) -> T {
        let Some(out) = out else { return run() };
        let start = now();
        let result = run();
        *out = Self { start, end: now() };
        result
    }

    pub fn elapsed(self) -> u64 {
        self.end.wrapping_sub(self.start)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GuestSample {
    pub interval: TickInterval,
    pub source: ExitSource,
}

#[derive(Clone, Copy, Debug)]
pub struct VcpuExit {
    pub kind: ExitKind,
    pub cause: usize,
    pub pc: usize,
    pub value: usize,
    pub deadline_reached: bool,
    pub native_event: bool,
}
pub trait ArchTimer {
    fn new() -> Self;
    fn now() -> u64;
    /// # Safety
    /// Exclusive IRQ-masked native owner; no out-of-band endpoint writers.
    unsafe fn arm(&mut self, deadline: u64) -> Result<()>;
    /// # Safety
    /// Same endpoint owner; called before ownership handoff.
    unsafe fn disarm(&mut self) -> Result<()>;
}
pub trait Resident<V: ArchVcpu>: Sized {
    /// # Safety
    /// Pinned native owner, IRQs masked until put/drop, no reentrant execution.
    unsafe fn new() -> Self;
    fn put(&mut self, vcpu: &mut V);
    fn load_interrupts(&mut self, _vcpu: &mut V, _chip: &mut V::Irq) -> Result<()> {
        Ok(())
    }
    fn put_interrupts(&mut self, _vcpu: &mut V, _chip: &mut V::Irq) -> Result<()> {
        Ok(())
    }
}
pub struct RunOptions<'a, V: ArchVcpu> {
    pub space: &'a V::Space,
    pub deadline: u64,
    pub io: &'a mut dyn GuestIo,
    pub timer: &'a mut V::Timer,
    pub resident: &'a mut V::Resident,
}
pub trait ArchVcpu: Send + Sized {
    type Space: Stage2Space;
    type Irq: VirtualIrqChip;
    type Timer: ArchTimer;
    type Resident: Resident<Self>;
    type Mailbox: Mailbox<Self>;
    fn create(
        space: &Self::Space,
        index: u8,
        epoch: u32,
        config: &VmConfig,
        boot: VcpuBoot,
    ) -> Result<Self>;
    fn reset_secondary(&mut self, space: &Self::Space, entry: usize, opaque: usize) -> Result<()>;
    /// # Safety
    /// Exclusive pinned native owner, IRQs masked, immutable retained space,
    /// armed absolute deadline; no lock spans entry. Put before foreign control.
    unsafe fn run(&mut self, options: RunOptions<'_, Self>) -> Result<VcpuExit>;
    /// Optional switch telemetry. The default measures the complete architecture
    /// call. Backends with exit emulation can bracket the native switch itself.
    /// # Safety
    /// Same exclusive native-owner and retained-space contract as `run`.
    unsafe fn run_with_trace(
        &mut self,
        options: RunOptions<'_, Self>,
        sample: Option<&mut GuestSample>,
    ) -> Result<VcpuExit> {
        if let Some(sample) = sample {
            sample.source = ExitSource::Other;
            TickInterval::measure(Some(&mut sample.interval), Self::Timer::now, || {
                // SAFETY: the caller retains the same run contract.
                unsafe { self.run(options) }
            })
        } else {
            // SAFETY: the caller retains the same run contract.
            unsafe { self.run(options) }
        }
    }
    fn set_external_irq(&mut self, asserted: bool);
    fn inject_ipi(&mut self);
    /// Earliest enabled timer that can end this vCPU's WFI, in absolute native ticks.
    fn wait_deadline(&self) -> Option<u64>;
    fn wake_pending(&mut self) -> bool;
    fn finish_control(&mut self, reply: ControlReply) -> Result<()>;
    /// # Safety
    /// Off-CPU context, matching pinned native owner, banks put, IRQs masked.
    unsafe fn synchronize(&mut self, instruction: bool);
    fn retire_local_translations(&mut self);
    fn translations_retired(&self) -> bool;
}
/// Only the first generations of a diagnostic epoch are retained; never a rolling history.
#[cfg(feature = "selftest")]
pub const FENCE_SAMPLE_LIMIT: usize = 32;
/// One target's completion. A missing path means the requester was not classified,
/// not that it returned directly (for example, a multi-target call or a cancellation).
#[cfg(feature = "selftest")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FenceSample {
    pub requester: u8,
    pub ticks: u64,
    pub deferred: Option<bool>,
}
pub trait Mailbox<V: ArchVcpu>: Send {
    type Ticket: Copy + Send;
    fn new(epoch: u32, index: u8, owner: usize) -> Self;
    /// Publish under the caller's mailbox guard. The caller drops the guard
    /// before kicking the pinned owner, which can drain before the kick returns.
    fn post_ipi(&self);
    /// Publish the fence and arm its requester together; the ticket is not an ACK.
    fn post_fence(&self, instruction: bool, requester: u8) -> Self::Ticket;
    /// True only after the owner's drain consumed the wake registration and
    /// fulfilled the fence. The successful waiter has nothing left to disarm.
    fn completed(&self, ticket: Self::Ticket) -> bool;
    /// One unambiguous target-side completion, with its observed requester path.
    /// Read under this epoch's mailbox guard. An absent or coalesced sample is not evidence.
    #[cfg(feature = "selftest")]
    fn fence_sample(&self, _instruction: bool, _generation: usize) -> Option<FenceSample> {
        None
    }
    /// Classify an exact single-remote-target reply while still holding its ACK guard.
    #[cfg(feature = "selftest")]
    fn note_fence_ready(&self, _ticket: Self::Ticket, _requester: u8, _deferred: bool) {}
    /// Cancel a registration on failed publication or abandoned wait; not an ACK.
    fn disarm(&self, requester: u8);
    fn pending(&self) -> bool;
    /// # Safety
    /// Matching pinned native owner, off-CPU context, banks put and IRQs masked.
    unsafe fn drain(&self, vcpu: &mut V) -> Result<usize>;
}
/// Exclusive observations of locally enabled pending IRQs after a native idle return.
/// These are snapshots, not attribution of the interrupt that woke the hart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum IdleWake {
    Timer,
    Software,
    External,
    Multiple,
    None,
}
/// A positive physical software-interrupt clear, not proof of who sent it.
#[cfg(feature = "selftest")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeAck {
    pub sequence: u64,
    pub ticks: u64,
    pub path: NativeAckPath,
}
#[cfg(feature = "selftest")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeAckPath {
    GuestExit,
    DispatchAck,
}
pub trait ArchSmp {
    /// Optional diagnostic read before global IRQ delivery resumes. No IRQ is acknowledged.
    const SAMPLE_IDLE: Option<fn() -> IdleWake> = None;
    /// Remotely readable baseline for one physical owner; not a per-VM receipt.
    #[cfg(feature = "selftest")]
    fn ack_sequence(_owner: usize) -> u64 {
        0
    }
    /// Sample on the IRQ-masked owner only, so no other ACK can split the record.
    #[cfg(feature = "selftest")]
    fn local_ack() -> Option<NativeAck> {
        None
    }
    fn cpu_id() -> usize;
    /// Signal this native owner after publishing work. A kick after its scan
    /// must remain observable through a concurrent transition to idle.
    fn kick(owner: usize) -> Result<()>;
    /// # Safety
    /// Retained unique worker stack/boot record; immutable runtime publication
    /// precedes start. Entry never returns and must install native CPU state.
    unsafe fn start(owner: usize, entry: extern "C" fn(usize) -> !, opaque: usize) -> Result<()>;
    /// # Safety
    /// Banks put, no locks held, wake sources enabled; the timer may be
    /// disarmed for an interrupt-only waiter. Do not consume pending kicks here.
    unsafe fn idle();
    /// Acknowledge before scanning published work, never between the scan and idle.
    fn acknowledge_kick();
}
pub trait HvArch: Send + Sync + 'static {
    type Vcpu: ArchVcpu<Space = Self::Space, Irq = Self::Irq, Timer = Self::Timer>;
    type Space: Stage2Space;
    type Irq: VirtualIrqChip;
    type Timer: ArchTimer;
    type Smp: ArchSmp;
    type IrqMask: LocalIrq;
    fn init(layout: GuestBoardLayout) -> core::result::Result<(), HvError>;
    /// Architecture-specific guest discovery, not the physical board's device tree.
    fn write_linux_dtb(
        config: &VmConfig,
        layout: GuestBoardLayout,
        hz: u32,
        args: &str,
        out: &mut [u8],
    ) -> Result<usize>;
    /// A board may stage a second canonical variant with host-supported extensions.
    /// Reshaping preserves the original template's variant and checks all bytes.
    fn write_extended_linux_dtb(
        config: &VmConfig,
        layout: GuestBoardLayout,
        hz: u32,
        args: &str,
        out: &mut [u8],
    ) -> Result<usize> {
        Self::write_linux_dtb(config, layout, hz, args, out)
    }
    fn publish_code();
}
