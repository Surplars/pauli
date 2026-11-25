//! Board wiring only: FDT/loader/native devices delegate all execution to pauli-core.
//! Ownership/Lock Ordering: prepare publishes once before SIE/HSM; no VM operations.
//! Invariants: logical CPU indices are mapped to real harts, including a nonzero boot hart.
use crate::{devices::Devices, fdt, irq, loader::Loader};
use pauli_arch_riscv::{
    host::{self, sbi},
    interface::{RiscvHvArch, smp},
};
use pauli_config::{BootConfig, MAX_BOOT_VMS, VmConfig};
use pauli_core::{
    BootStorage, HvError, Result,
    board::{Board, BoardInfo, VmResources},
    memory::pool::DomainPool,
};
use pauli_hal::{
    device::{HostBlockDevice, HostNetDevice, HostRtcDevice},
    fdt::Fdt,
    mem::PhysRange,
    sync::{Once, SpinLock},
};
static BOARD: Once<QemuVirtBoard> = Once::new();
pub static RUNTIME: BootStorage<RiscvHvArch, QemuVirtBoard> = BootStorage::new();
pub struct QemuVirtBoard {
    info: BoardInfo,
    boot: BootConfig,
    resources: SpinLock<[Option<VmResources>; MAX_BOOT_VMS]>,
    pool: DomainPool,
    devices: Devices,
}
impl Board for QemuVirtBoard {
    // Devices::initialize requires the native UART's IRQ route before boot.
    const IRQ_DRIVEN_CONSOLE: bool = true;
    const COLLECT_HS_STATS: bool = cfg!(feature = "selftest");
    const UNCONTENDED_QUANTUM_US: Option<u64> = Some(20_000);

    fn info(&self) -> BoardInfo {
        self.info
    }
    fn load_boot_config(&self) -> Result<&'static BootConfig> {
        Ok(&BOARD.get().ok_or(HvError::InvalidState)?.boot)
    }
    fn take_resources(&self, slot: usize, _vm: &VmConfig) -> Result<VmResources> {
        self.resources
            .lock()
            .get_mut(slot)
            .and_then(Option::take)
            .ok_or(HvError::InvalidMemory)
    }
    fn domain_pool(&self) -> Option<&DomainPool> {
        Some(&self.pool)
    }
    fn resolve_block(&self, slot: u8) -> Result<&'static dyn HostBlockDevice> {
        self.devices.block(slot)
    }
    fn resolve_uplink_net(&self) -> Result<&'static dyn HostNetDevice> {
        self.devices.net()
    }
    fn rtc(&self) -> Option<&'static dyn HostRtcDevice> {
        self.devices.rtc.map(|rtc| rtc as &dyn HostRtcDevice)
    }
    fn pop_console_rx(&self) -> Option<u8> {
        self.devices.input()
    }
    fn console_rx_queued(&self) -> bool {
        self.devices.input_queued()
    }
    fn service_devices(&self, now: u64) {
        self.devices.service(now);
    }
    fn shutdown(&self) -> ! {
        poweroff()
    }
}
pub fn prepare(hart: usize, dtb: usize) -> Result<&'static QemuVirtBoard> {
    unsafe extern "C" {
        static __hv_begin: u8;
        static __hv_end: u8;
    }
    // SAFETY: OpenSBI's a1 is the retained native QEMU FDT handoff allocation.
    let blob = unsafe { fdt::firmware_blob(dtb) }?;
    let tree = fdt::parsed(Fdt::new(blob))?;
    fdt::check_soc(tree)?;
    let topology = fdt::discover(tree, hart)?;
    preflight(topology.count)?;
    let end = &raw const __hv_end as usize;
    let ram = fdt::memory(tree, &raw const __hv_begin as usize)?;
    let firmware = PhysRange::new(dtb, blob.len()).map_err(|_| HvError::InvalidMemory)?;
    // SAFETY: boot runs before any other native owner; FDT resolved its unique logical index.
    unsafe {
        host::install_cpu_id(topology.boot);
        host::trap::install_vector();
    }
    pauli_arch_riscv::init_capabilities(true, topology.fd, topology.aia);
    // SAFETY: boot owns its compare with local IRQs masked, tp and trap vector installed.
    unsafe { pauli_arch_riscv::init_local_sstc() };
    let (root, guest_ids) = irq::Root::discover(tree, &topology, ram)?;
    #[cfg(feature = "aia-guest-file")]
    // SAFETY: FDT established a supported S-file topology; guest file 1 is reserved.
    unsafe {
        pauli_arch_riscv::aia::hardware::init_owner(guest_ids);
    }
    #[cfg(feature = "selftest")]
    acceptance(tree, ram, &root, topology.hz)?;
    let mut loader = Loader::open(tree, ram, end, firmware)?;
    if !topology.fd
        && loader
            .config
            .vms
            .iter()
            .flatten()
            .any(|vm| vm.firmware.is_none())
    {
        return Err(HvError::Unsupported);
    }
    // One host-only reservation for actual workers; CPU stacks never enter guest RAM.
    let bytes = smp::worker_stack_bytes(topology.count)?;
    let stacks = if bytes == 0 {
        &mut [][..]
    } else {
        loader.take_bytes(bytes, pauli_hal::addr::PAGE_SIZE)?
    };
    smp::install_with_stacks(&topology.harts[..topology.count], guest_ids, stacks)?;
    let resources = loader.resources(&topology)?;
    let (devices, routes) = Devices::initialize(tree, ram, Some(&mut loader), &root)?;
    irq::ROUTER.set((root, routes)).map_err(|_| HvError::Busy)?;
    host::trap::set_handler(irq::event).map_err(|_| HvError::Busy)?;
    host::irq::enable_wake_sources(true);
    // SAFETY: native vector/IRQ controller and all routes are initialized.
    unsafe {
        host::irq::enable();
    }
    let info = BoardInfo {
        ram_base: ram.base(),
        ram_size: ram.size(),
        kernel_end: end,
        timebase_hz: topology.hz,
        host_cpus: topology.count,
        boot_cpu: topology.boot,
    };
    pauli_hal::hv_info!(
        "QEMU virt: {} harts, boot {}, {} Hz",
        info.host_cpus,
        info.boot_cpu,
        info.timebase_hz
    );
    BOARD
        .set(QemuVirtBoard {
            info,
            boot: loader.config,
            resources: SpinLock::new(resources),
            // SAFETY: firmware, host, templates, static guests and native DMA
            // were reserved by this loader; no other allocator retains its pool.
            pool: unsafe { DomainPool::from_boot(loader.pool) },
            devices,
        })
        .map_err(|_| HvError::Busy)?;
    BOARD.get().ok_or(HvError::InvalidState)
}
fn preflight(cpus: usize) -> Result<()> {
    if !sbi::probe(sbi::TIME) || (cpus > 1 && !(sbi::probe(sbi::HSM) && sbi::probe(sbi::IPI))) {
        return Err(HvError::Unsupported);
    }
    Ok(())
}
#[cfg(feature = "selftest")]
fn acceptance(tree: Fdt<'_>, ram: PhysRange, root: &irq::Root, hz: u64) -> Result<()> {
    let Some(chosen) = fdt::parsed(tree.find("/chosen"))? else {
        return Ok(());
    };
    if fdt::parsed(chosen.text("bootargs"))? != Some("pauli.selftest") {
        return Ok(());
    }
    let _ = Devices::initialize(tree, ram, None, root)?;
    // SAFETY: H and timer were validated before this single-owner acceptance;
    // no worker or runtime is published and the probe owns its dedicated pages.
    unsafe {
        pauli_arch_riscv::selftest::probe(pauli_config::GuestBoardLayout::DEFAULT.ram_base as usize, hz)
    }
    .map_err(pauli_arch_riscv::interface::error)?;
    pauli_hal::hv_info!(
        "HV selftest PASS: entry/resume, native timer preemption, G-stage RX isolation"
    );
    poweroff()
}
pub fn poweroff() -> ! {
    let _ = sbi::shutdown();
    // OpenSBI may report SRST unavailable. No use of a guessed device address.
    loop {
        core::hint::spin_loop();
    }
}
