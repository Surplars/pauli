//! Explicit board/ISA resource boundary for production runtime host tests.
//! Ownership/Lock Ordering: resources transfer once; runtime drops before allocated RAM.
//! Invariants: allocator-owned aligned RAM is foreign while GuestRam owns it.
use super::model_cpu::{Cpu, Mask};
use crate::{
    HvError, Result,
    arch::*,
    board::*,
    memory::pool::{DomainPool, FramePool},
    memory::{Arena, GuestRam},
    runtime::VmRuntime,
};
use pauli_config::{BootConfig, VmConfig, VmImage};
use pauli_hal::{device::*, mem::PhysRange, sync::SpinLock};
use std::{
    alloc::{Layout, alloc_zeroed, dealloc},
    boxed::Box,
    cell::{Cell, RefCell},
    collections::VecDeque,
    vec,
};
std::thread_local! {
    pub static OWNER: Cell<usize> = const { Cell::new(0) };
    // One bit per physical owner that completed its Sstc probe.
    pub(super) static SSTC_OWNERS: Cell<usize> = const { Cell::new(usize::MAX) };
    pub static NOW: Cell<u64> = const { Cell::new(0) };
    pub(super) static CLOCK_READ: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
    pub(super) static WORKER_STARTS: Cell<u64> = const { Cell::new(0) };
    pub(super) static REJECT_WORKER_START: Cell<Option<usize>> = const { Cell::new(None) };
    pub static KICKS: Cell<u64> = const { Cell::new(0) };
    pub(super) static KICKED_OWNERS: Cell<usize> = const { Cell::new(0) };
    static PENDING_KICKS: Cell<usize> = const { Cell::new(0) };
    #[cfg(feature = "selftest")]
    static NATIVE_ACK: [Cell<Option<NativeAck>>; MAX_CPUS] = const { [const { Cell::new(None) }; MAX_CPUS] };
    pub(super) static IDLE_KICK: Cell<bool> = const { Cell::new(false) };
    pub(super) static IDLE_DEADLINE: Cell<Option<u64>> = const { Cell::new(None) };
    pub(super) static IDLE_ENTRIES: Cell<usize> = const { Cell::new(0) };
    #[cfg(feature = "selftest")]
    pub(super) static IDLE_RETURN: Cell<Option<(u64, IdleWake)>> = const { Cell::new(None) };
    pub(super) static REJECT_KICK_ONCE: Cell<Option<usize>> = const { Cell::new(None) };
    // One sender can model a receiver running while its native kick is still in flight.
    pub(super) static KICK_IN_FLIGHT: Cell<Option<Box<dyn FnOnce(usize)>>> = const { Cell::new(None) };
    pub(super) static REJECT_NEXT_SPACE: Cell<bool> = const { Cell::new(false) };
    pub(super) static REJECT_NEXT_IRQ_CHIP: Cell<bool> = const { Cell::new(false) };
}
pub struct Arch;
pub struct Timer;
impl ArchTimer for Timer {
    fn new() -> Self {
        Self
    }
    fn now() -> u64 {
        CLOCK_READ.with(|hook| {
            if let Some(hook) = hook.borrow_mut().as_mut() {
                hook();
            }
        });
        NOW.with(Cell::get)
    }
    unsafe fn arm(&mut self, deadline: u64) -> Result<()> {
        IDLE_DEADLINE.with(|t| t.set(Some(deadline)));
        Ok(())
    }
    unsafe fn disarm(&mut self) -> Result<()> {
        IDLE_DEADLINE.with(|t| t.set(None));
        Ok(())
    }
}
pub struct Smp;
impl ArchSmp for Smp {
    #[cfg(feature = "selftest")]
    fn ack_sequence(owner: usize) -> u64 {
        NATIVE_ACK.with(|a| a[owner].get().map_or(0, |x| x.sequence))
    }
    #[cfg(feature = "selftest")]
    fn local_ack() -> Option<NativeAck> {
        NATIVE_ACK.with(|a| a[Self::cpu_id()].get())
    }
    #[cfg(feature = "selftest")]
    const SAMPLE_IDLE: Option<fn() -> IdleWake> = Some(|| {
        if let Some((_, source)) = IDLE_RETURN.with(Cell::take) {
            source
        } else if IDLE_KICK.with(Cell::get) {
            IdleWake::Software
        } else {
            IdleWake::Timer
        }
    });
    fn cpu_id() -> usize {
        OWNER.with(Cell::get)
    }
    fn hart_id(owner: usize) -> Option<usize> {
        // Keep physical IDs distinct from logical indices in diagnostic tests.
        Some(0x40 + 2 * owner)
    }
    fn kick_detailed(owner: usize) -> core::result::Result<(), SmpError> {
        KICKS.with(|k| k.set(k.get() + 1));
        KICKED_OWNERS.with(|k| k.set(k.get() | (1 << owner)));
        if REJECT_KICK_ONCE.with(|o| o.get() == Some(owner)) {
            REJECT_KICK_ONCE.with(|o| o.set(None));
            return Err(SmpError {
                error: HvError::Fault,
                hart: Self::hart_id(owner),
                raw: Some(-4),
            });
        }
        PENDING_KICKS.with(|k| k.set(k.get() | (1 << owner)));
        if let Some(hook) = KICK_IN_FLIGHT.with(Cell::take) {
            hook(owner);
        }
        Ok(())
    }
    unsafe fn start(
        owner: usize,
        _entry: extern "C" fn(usize) -> !,
        _opaque: usize,
    ) -> core::result::Result<(), SmpError> {
        WORKER_STARTS.with(|starts| starts.set(starts.get() | (1u64 << owner)));
        if REJECT_WORKER_START.with(|reject| reject.get() == Some(owner)) {
            return Err(SmpError {
                error: HvError::Fault,
                hart: Self::hart_id(owner),
                raw: Some(-6),
            });
        }
        Ok(())
    }
    unsafe fn idle() {
        IDLE_KICK.with(|w| w.set(PENDING_KICKS.with(|k| k.get() & (1 << Self::cpu_id()) != 0)));
        IDLE_ENTRIES.with(|n| n.set(n.get() + 1));
        #[cfg(feature = "selftest")]
        if let Some((at, _)) = IDLE_RETURN.with(Cell::get) {
            NOW.with(|now| now.set(at));
        }
    }
    fn acknowledge_kick() {
        let owner = Self::cpu_id();
        let _old = PENDING_KICKS.with(|k| k.replace(k.get() & !(1 << owner)));
        #[cfg(feature = "selftest")]
        if _old & (1 << owner) != 0 {
            let sample = NativeAck {
                sequence: Self::ack_sequence(owner) + 1,
                ticks: Timer::now(),
                path: NativeAckPath::DispatchAck,
            };
            NATIVE_ACK.with(|a| a[owner].set(Some(sample)));
        }
    }
}
pub struct Space {
    base: usize,
    end: usize,
}
// SAFETY: the fixture stores GPA bounds only, never an arena pointer.
unsafe impl Stage2Space for Space {
    unsafe fn create(ram: &GuestRam, _arena: &mut Arena, _payload: Payload) -> Result<Self> {
        if REJECT_NEXT_SPACE.with(|reject| reject.replace(false)) {
            return Err(HvError::Fault);
        }
        Ok(Self {
            base: ram.guest_base(),
            end: ram.guest_base() + ram.len(),
        })
    }
    fn executable(&self, address: usize) -> bool {
        address & 1 == 0 && address >= self.base && address < self.end
    }
}
#[derive(Default)]
pub struct Irqs {
    pub lines: [bool; MAX_VCPUS],
    pub uart_assertions: usize,
    pub samples: Cell<usize>,
    pub loads: usize,
    pub puts: usize,
    pub split_snapshots: usize,
    pub fail_load: bool,
}
impl VirtualIrqChip for Irqs {
    fn create(_cpus: u8, _advanced: bool) -> Result<Self> {
        if REJECT_NEXT_IRQ_CHIP.with(Cell::take) {
            return Err(HvError::Unsupported);
        }
        Ok(Self::default())
    }
    fn access(
        &mut self,
        _region: IrqRegion,
        _offset: usize,
        _write: Option<usize>,
    ) -> Option<usize> {
        Some(0)
    }
    fn register(&mut self, _cpu: u8, _access: RegisterAccess) -> Option<usize> {
        Some(0)
    }
    fn set_level(&mut self, source: u32, asserted: bool) {
        if asserted && source == pauli_config::GuestBoardLayout::DEFAULT.uart_irq {
            self.uart_assertions += 1;
        }
        self.lines[0] = asserted;
    }
    fn pending(&self, cpu: u8) -> bool {
        self.samples.set(self.samples.get() + 1);
        self.lines[cpu as usize]
    }
    fn quiesce(&mut self) -> Result<()> {
        self.lines.fill(false);
        Ok(())
    }
}
impl HvArch for Arch {
    type Vcpu = Cpu;
    type Space = Space;
    type Irq = Irqs;
    type Timer = Timer;
    type Smp = Smp;
    type IrqMask = Mask;
    fn sstc_available_for_owners(owners: usize) -> bool {
        owners != 0 && SSTC_OWNERS.with(|available| available.get() & owners == owners)
    }
    fn guest_sstc(dtb: &[u8], vcpus: u8) -> Result<bool> {
        if dtb == [0xd1; 64] {
            // Non-DTB fixture used by the scheduling and storage tests.
            return Ok(false);
        }
        pauli_config::guest_dtb::riscv_sstc(dtb, vcpus).map_err(|_| HvError::InvalidImage)
    }
    fn write_linux_dtb(
        config: &VmConfig,
        layout: pauli_config::GuestBoardLayout,
        hz: u32,
        args: &str,
        out: &mut [u8],
    ) -> Result<usize> {
        pauli_config::guest_dtb::riscv(config, layout, hz, args, out)
            .map_err(|_| HvError::InvalidImage)
    }
    fn write_extended_linux_dtb(
        config: &pauli_config::VmConfig,
        layout: pauli_config::GuestBoardLayout,
        hz: u32,
        args: &str,
        out: &mut [u8],
    ) -> Result<usize> {
        pauli_config::guest_dtb::riscv_with_sstc(config, layout, hz, args, true, out)
            .map_err(|_| HvError::InvalidImage)
    }
    fn init(_layout: pauli_config::GuestBoardLayout) -> Result<()> {
        Ok(())
    }
    fn publish_code() {}
}
pub struct TestBoard {
    pub info: BoardInfo,
    pub(super) input: SpinLock<VecDeque<u8>>,
    pub(super) direct_linux: core::sync::atomic::AtomicBool,
    #[cfg(feature = "guest-control")]
    pub(super) boot_service: core::sync::atomic::AtomicBool,
    #[cfg(feature = "guest-control")]
    pub(super) boot_quota: Option<crate::manager::Budget>,
    config: &'static BootConfig,
    resources: SpinLock<[Option<VmResources>; 4]>,
    pool: Option<DomainPool>,
    disks: [Option<&'static dyn HostBlockDevice>; pauli_config::MAX_DISKS],
}
impl Board for TestBoard {
    const IRQ_DRIVEN_CONSOLE: bool = true;

    fn info(&self) -> BoardInfo {
        self.info
    }
    fn load_boot_config(&self) -> Result<&'static BootConfig> {
        Ok(self.config)
    }
    #[cfg(feature = "guest-control")]
    fn boot_service_for(&self, slot: usize) -> Option<usize> {
        (slot == 0
            && self
                .boot_service
                .load(core::sync::atomic::Ordering::Relaxed))
        .then_some(1)
    }
    #[cfg(feature = "guest-control")]
    fn boot_service_quota(&self, slot: usize) -> Option<crate::manager::Budget> {
        (slot == 0).then_some(self.boot_quota).flatten()
    }
    fn take_resources(&self, slot: usize, _config: &VmConfig) -> Result<VmResources> {
        self.resources.lock()[slot].take().ok_or(HvError::Busy)
    }
    fn domain_pool(&self) -> Option<&DomainPool> {
        self.pool.as_ref()
    }
    fn resolve_block(&self, slot: u8) -> Result<&'static dyn HostBlockDevice> {
        self.disks
            .get(usize::from(slot))
            .copied()
            .flatten()
            .ok_or(HvError::Unsupported)
    }
    fn resolve_uplink_net(&self) -> Result<&'static dyn HostNetDevice> {
        Err(HvError::Unsupported)
    }
    fn rtc(&self) -> Option<&'static dyn HostRtcDevice> {
        None
    }
    fn pop_console_rx(&self) -> Option<u8> {
        self.input.lock().pop_front()
    }
    fn direct_linux_console(&self) -> bool {
        self.direct_linux
            .load(core::sync::atomic::Ordering::Relaxed)
    }
    fn console_rx_queued(&self) -> bool {
        assert!(Mask::active(), "RX probe must be inside the WFI mask");
        !self.input.lock().is_empty()
    }
    fn service_devices(&self, _now: u64) {}
    fn shutdown(&self) -> ! {
        panic!("test unexpectedly shut down")
    }
}
pub struct Allocation {
    pointer: *mut u8,
    layout: Layout,
}
impl Allocation {
    /// BoardInfo describes enclosing addresses; the pool only owns the spare bank.
    fn bounds<'a>(banks: impl Iterator<Item = &'a Self>) -> (usize, usize) {
        banks.fold((usize::MAX, 0), |(low, high), bank| {
            let base = bank.pointer as usize;
            (low.min(base), high.max(base + bank.layout.size()))
        })
    }
    fn domain_pool(&self) -> DomainPool {
        let frames = FramePool::new(self.pointer as usize, self.layout.size()).unwrap();
        // SAFETY: this is a separate host allocation, retained throughout the fixture.
        unsafe { DomainPool::from_boot(frames) }
    }
    fn new(bytes: usize) -> Self {
        let layout = Layout::from_size_align(bytes, pauli_config::ALIGN as usize).unwrap();
        // SAFETY: valid nonzero layout; test checks allocator success before use.
        let pointer = unsafe { alloc_zeroed(layout) };
        assert!(!pointer.is_null());
        Self { pointer, layout }
    }
}
impl Drop for Allocation {
    fn drop(&mut self) {
        // SAFETY: Fixture declares runtime first, so all contexts/mappings are
        // dropped before these private allocations, and no native worker exists.
        unsafe {
            dealloc(self.pointer, self.layout);
        }
    }
}
pub struct Fixture {
    pub runtime: VmRuntime<Arch, TestBoard>,
    _backing: [Allocation; 2],
    _spare: Option<Allocation>,
}
impl Fixture {
    pub fn new(host_cpus: usize, guest_cpus: u8) -> Self {
        Self::with_vcpus(host_cpus, [guest_cpus; 2])
    }
    pub fn with_vcpus(host_cpus: usize, guest_cpus: [u8; 2]) -> Self {
        Self::with_pool(host_cpus, guest_cpus, 0)
    }
    pub fn with_pool(host_cpus: usize, guest_cpus: [u8; 2], spare_mib: usize) -> Self {
        Self::with_config(host_cpus, guest_cpus, spare_mib, |_, _| {})
    }
    pub fn with_config(
        host_cpus: usize,
        guest_cpus: [u8; 2],
        spare_mib: usize,
        configure: impl Fn(usize, &mut VmConfig),
    ) -> Self {
        Self::with_templates(host_cpus, guest_cpus, spare_mib, |slot, vm| {
            configure(slot, vm);
            BootImages {
                kernel: if slot == 0 { &[0x31; 64] } else { &[0x32; 64] },
                initrd: &[],
                dtb: &[0xd1; 64],
            }
        })
    }
    pub fn with_templates(
        host_cpus: usize,
        guest_cpus: [u8; 2],
        spare_mib: usize,
        template: impl Fn(usize, &mut VmConfig) -> BootImages,
    ) -> Self {
        Self::with_disks(
            host_cpus,
            guest_cpus,
            spare_mib,
            [None; pauli_config::MAX_DISKS],
            template,
        )
    }
    pub fn with_disks(
        host_cpus: usize,
        guest_cpus: [u8; 2],
        spare_mib: usize,
        disks: [Option<(pauli_config::DiskConfig, &'static dyn HostBlockDevice)>;
            pauli_config::MAX_DISKS],
        template: impl Fn(usize, &mut VmConfig) -> BootImages,
    ) -> Self {
        let (board, allocations, spare) =
            Self::test_board(host_cpus, guest_cpus, spare_mib, disks, template);
        Self::from_board(board, allocations, spare)
    }
    pub(super) fn from_board(
        board: TestBoard,
        allocations: [Allocation; 2],
        spare: Option<Allocation>,
    ) -> Self {
        let board = Box::leak(Box::new(board));
        Self {
            runtime: VmRuntime::create(board, Box::leak(Box::new(crate::manager::VmStore::new())))
                .unwrap(),
            _backing: allocations,
            _spare: spare,
        }
    }
    pub(super) fn test_board(
        host_cpus: usize,
        guest_cpus: [u8; 2],
        spare_mib: usize,
        disks: [Option<(pauli_config::DiskConfig, &'static dyn HostBlockDevice)>;
            pauli_config::MAX_DISKS],
        template: impl Fn(usize, &mut VmConfig) -> BootImages,
    ) -> (TestBoard, [Allocation; 2], Option<Allocation>) {
        NOW.with(|n| n.set(0));
        CLOCK_READ.with(|hook| hook.take());
        WORKER_STARTS.with(|starts| starts.set(0));
        REJECT_WORKER_START.with(|reject| reject.set(None));
        PENDING_KICKS.with(|k| k.set(0));
        #[cfg(feature = "selftest")]
        NATIVE_ACK.with(|a| a.iter().for_each(|v| v.set(None)));
        #[cfg(feature = "selftest")]
        IDLE_RETURN.with(|r| r.set(None));
        OWNER.with(|o| o.set(0));
        let spare = (spare_mib != 0).then(|| Allocation::new(spare_mib << 20));
        let pool = spare.as_ref().map(Allocation::domain_pool);
        let mut resources = [const { None }; 4];
        let mut vms = [None; 4];
        let mut dtb_size = 0;
        let allocations = core::array::from_fn(|slot| {
            let mut vm = VmConfig::new(if slot == 0 { "alpha" } else { "beta" }).unwrap();
            vm.memory_size = 64 * 1024 * 1024;
            vm.image = VmImage {
                kernel_source: 16 * 1024 * 1024 + slot as u64 * 4096,
                kernel_size: 64,
                kernel_memory_size: 128,
                initrd_source: 0,
                initrd_size: 0,
            };
            vm.vcpus = guest_cpus[slot];
            vm.quantum_us = 300;
            let images = template(slot, &mut vm);
            vm.dtb_offset = dtb_size;
            vm.dtb_size = images.dtb.len() as u64;
            dtb_size += vm.dtb_size;
            let allocation = Allocation::new(vm.memory_size as usize);
            vm.host_base = allocation.pointer as u64;
            let range = PhysRange::new(vm.host_base as usize, vm.memory_size as usize).unwrap();
            // SAFETY: aligned allocation transfers exclusively to this VM as foreign RAM.
            let ram = unsafe { GuestRam::new(range, vm.guest_base as usize) }.unwrap();
            resources[slot] = Some(VmResources {
                ram,
                arena: boot_arena(&vm),
                images,
            });
            vms[slot] = Some(vm);
            allocation
        });
        let (low, high) = Allocation::bounds(allocations.iter().chain(spare.iter()));
        let config = Box::leak(Box::new(BootConfig {
            template_base: 16 * 1024 * 1024,
            template_size: 3 * 4096,
            dtb_size,
            management: true,
            smoke_payload: false,
            disks: disks.map(|entry| entry.map(|(config, _)| config)),
            vms,
        }));
        let board = TestBoard {
            info: BoardInfo {
                ram_base: low - 4096,
                ram_size: high - low + 4096,
                kernel_end: low,
                timebase_hz: 1_000_000,
                host_cpus,
                boot_cpu: 0,
            },
            config,
            input: SpinLock::new(VecDeque::new()),
            direct_linux: core::sync::atomic::AtomicBool::new(false),
            #[cfg(feature = "guest-control")]
            boot_service: false.into(),
            #[cfg(feature = "guest-control")]
            boot_quota: None,
            resources: SpinLock::new(resources),
            pool,
            disks: disks.map(|entry| entry.map(|(_, disk)| disk)),
        };
        (board, allocations, spare)
    }
}

fn boot_arena(vm: &VmConfig) -> Arena {
    let storage = if vm.disk.is_some() {
        crate::backends::block::BUFFER_BYTES
    } else {
        0
    };
    let size = 64 + storage + crate::vm::Vm::<Arch>::vcpu_arena_bytes(vm.vcpus).unwrap();
    Arena::new(Box::leak(vec![0; size].into_boxed_slice()))
}
