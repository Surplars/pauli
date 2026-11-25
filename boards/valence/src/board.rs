//! Wave8 single-hart HS host; physical devices are used only after FDT validation.
//! Ownership: boot owns the S-file and UART; each boot slot gets private backing.
//! The firmware, native handoff, immutable sources and RPU/GEM never map into VS.
use super::assignment::{Arenas, Slots};
use super::handoff::{
    self, IMSIC_IDS, RAM_BASE, RAM_BYTES, S_APLIC, TIMEBASE_HZ, UART, UART_BAUD, UART_CLOCK,
    UART_IRQ,
};
use core::mem::MaybeUninit;
use pauli_arch_riscv::{
    host::{
        self,
        aplic::{Aplic, SOURCE_LEVEL_HIGH},
        imsic,
        irq::RiscvIrq,
        sbi,
        trap::Event,
    },
    interface::{RiscvHvArch, smp},
};
use pauli_config::{BootConfig, GuestBoardLayout, MAX_GUEST_DTB, VmConfig};
use pauli_core::{
    BootStorage, HvError, Result,
    board::{Board, BoardInfo, BootImages, Payload, VmResources},
};
use pauli_hal::{
    addr::PAGE_SIZE,
    device::{DeviceResource, HostCharDevice},
    irq::IrqHandler,
    log::Console,
    mem::PhysRange,
    sync::{Once, SpinLock},
};
const UART_DIVISOR: u16 = (UART_CLOCK / (16 * UART_BAUD)) as u16;
// NOLOAD arrays, not one-byte linker sentinels: Rust and the ELF reserve
// the same extents. No reference exists until the sole boot owner initializes them.
#[unsafe(link_section = ".wave8.arena")]
#[unsafe(export_name = "__guest_arena")]
static mut GUEST_ARENA: Arenas = [const { MaybeUninit::uninit() }; 2];
#[unsafe(link_section = ".wave8.page")]
#[unsafe(export_name = "__guest_page")]
static mut GUEST_PAGE: MaybeUninit<[u8; PAGE_SIZE]> = MaybeUninit::uninit();
// Private to HS. Filled once for Linux, then retained as a read-only restart source.
static mut GUEST_DTB: [[u8; MAX_GUEST_DTB]; 2] = [[0; MAX_GUEST_DTB]; 2];
type Uart = pauli_ns16550a::Ns16550a<RiscvIrq>;
static SERIAL: Once<Uart> = Once::new();
static ROOT: Once<Aplic> = Once::new();
static BOARD: Once<Wave8> = Once::new();
pub static RUNTIME: BootStorage<RiscvHvArch, Wave8> = BootStorage::new();
pub struct Wave8 {
    info: BoardInfo,
    layout: GuestBoardLayout,
    boot: BootConfig,
    payload: Payload,
    guests: SpinLock<Slots>,
}
impl Board for Wave8 {
    const IRQ_DRIVEN_CONSOLE: bool = true;
    const UNCONTENDED_QUANTUM_US: Option<u64> = Some(100_000);
    const COLLECT_HS_STATS: bool = true;
    const IDLE_USE_GUEST_DEADLINE: bool = true;
    fn info(&self) -> BoardInfo {
        self.info
    }
    fn layout(&self) -> GuestBoardLayout {
        self.layout
    }
    fn payload(&self) -> Payload {
        self.payload
    }
    fn load_boot_config(&self) -> Result<&'static BootConfig> {
        Ok(&BOARD.get().ok_or(HvError::InvalidState)?.boot)
    }
    fn boot_service_for(&self, manager_slot: usize) -> Option<usize> {
        // Only the native manager handoff installs the fixed manager/service pair.
        (self.boot.management && manager_slot == 0).then_some(1)
    }
    fn take_resources(&self, slot: usize, _config: &VmConfig) -> Result<VmResources> {
        self.guests.lock().take(slot)
    }
    fn pop_console_rx(&self) -> Option<u8> {
        SERIAL.get()?.read_byte()
    }
    fn direct_linux_console(&self) -> bool {
        self.payload == Payload::Linux
    }
    fn console_rx_queued(&self) -> bool {
        SERIAL.get().is_some_and(Uart::has_rx)
    }
    fn service_devices(&self, _now: u64) {}
    fn shutdown(&self) -> ! {
        // Only an explicit host shutdown reaches here; guest SRST stops its VM.
        let _ = flush_console();
        let _ = sbi::shutdown();
        halt()
    }
}
/// # Safety
/// Called once from OpenSBI with its retained FDT and exclusive boot-time resources.
pub unsafe fn prepare(hart: usize, dtb: usize) -> Result<&'static Wave8> {
    // SAFETY: startup installed our stack/tp, disabled IRQs, and preserved a0/a1.
    unsafe { host::trap::install_vector() };
    // SAFETY: firmware supplied the native blob; validate its extent and board identity before MMIO.
    let handoff = unsafe { handoff::validate(hart, dtb)? };
    if !sbi::probe(sbi::TIME) {
        return Err(HvError::Unsupported);
    }
    let guest_ids = handoff.guest_file_ids();
    smp::install_topology(&[hart], guest_ids)?;
    pauli_arch_riscv::init_capabilities(true, true, true);
    #[cfg(feature = "aia-guest-file")]
    // SAFETY: the validated guest-file window belongs to the sole HS owner,
    // with IRQs masked. init_owner checks file 1 is implemented and unused.
    let aia_file = unsafe { pauli_arch_riscv::aia::hardware::init_owner(guest_ids) };
    #[cfg(not(feature = "aia-guest-file"))]
    let aia_file = false;
    // SAFETY: validated single-hart OpenSBI board, before IRQ or dispatcher startup.
    unsafe { pauli_arch_riscv::init_boot_sstc(1) };
    pauli_core::trace::set_sstc_active(pauli_arch_riscv::sstc_available());
    // SAFETY: validated Wave8 DT and boot privilege; only the S-domain and S-file are used.
    unsafe { install_devices()? };
    unsafe extern "C" {
        static __hv_end: u8;
    }
    let end = &raw const __hv_end as usize;
    let layout = handoff.layout();
    // SAFETY: only the boot hart consumes the validated firmware loans and static storage.
    let (payload, boot, guests) = unsafe { resources(handoff.external, layout)? };
    let info = BoardInfo {
        ram_base: RAM_BASE,
        ram_size: RAM_BYTES,
        kernel_end: end,
        timebase_hz: TIMEBASE_HZ,
        host_cpus: 1,
        boot_cpu: 0,
    };
    pauli_hal::hv_info!(
        "Valence: HS hart 0, timebase {} Hz, {:?} guest, sstc={}, aia_file={}, stats={:#x}\r",
        TIMEBASE_HZ,
        payload,
        pauli_arch_riscv::sstc_available(),
        aia_file,
        &raw const pauli_core::trace::PAULI_HS_STATS as usize
    );
    BOARD
        .set(Wave8 {
            info,
            layout,
            boot,
            payload,
            guests: SpinLock::new(guests),
        })
        .map_err(|_| HvError::Busy)?;
    BOARD.get().ok_or(HvError::InvalidState)
}
unsafe fn install_devices() -> Result<()> {
    let resource = DeviceResource {
        base: UART,
        size: 8,
        irq: Some(UART_IRQ),
    };
    // SAFETY: validated byte-wide Wave8 UART; no guest sees this native MMIO.
    let uart = unsafe { Uart::new(resource, 0, 1, UART_DIVISOR) }?;
    SERIAL.set(uart).map_err(|_| HvError::Busy)?;
    // SAFETY: OpenSBI owns M-domain MSI destinations; IRQ 3 is delegated to this S-domain.
    let root = unsafe {
        imsic::init(IMSIC_IDS)?;
        Aplic::new(
            DeviceResource {
                base: S_APLIC,
                size: 0x4000,
                irq: None,
            },
            31,
            true,
        )?
    };
    root.route(UART_IRQ, 0, UART_IRQ, SOURCE_LEVEL_HIGH)?;
    // SAFETY: the native S-file and UART identity are exclusive to hart zero.
    unsafe {
        imsic::set_enabled(UART_IRQ as usize, true);
    }
    root.set_enabled(UART_IRQ, true)?;
    root.rearm_level(UART_IRQ)?;
    ROOT.set(root).map_err(|_| HvError::Busy)?;
    host::trap::set_handler(event).map_err(|_| HvError::Busy)?;
    host::irq::enable_wake_sources(true);
    // SAFETY: the installed HS vector owns external and native timer/SSIP traps.
    unsafe {
        host::irq::enable();
    }
    if let Some(uart) = SERIAL.get() {
        uart.enable_async_tx();
    }
    Ok(())
}
fn event(event: Event) {
    if !matches!(event, Event::External) {
        return;
    }
    // SAFETY: this vector runs on the sole native boot owner's S-file.
    let id = unsafe { imsic::claim() };
    if let (UART_IRQ, Some(uart), Some(root)) = (id, SERIAL.get(), ROOT.get()) {
        uart.handle_irq();
        let _ = root.rearm_level(UART_IRQ);
    }
}
fn embedded() -> Result<(BootConfig, BootImages)> {
    let kernel = crate::guest::image();
    let page = (&raw mut GUEST_PAGE) as usize;
    let memory = PhysRange::new(page, PAGE_SIZE).map_err(|_| HvError::InvalidMemory)?;
    let mut vm = VmConfig::new("wave8").map_err(|_| HvError::InvalidConfig)?;
    vm.host_base = memory.base() as u64;
    vm.memory_size = memory.size() as u64;
    vm.image.kernel_size = kernel.len() as u64;
    vm.image.kernel_memory_size = vm.image.kernel_size;
    vm.autostart = true;
    let boot = BootConfig {
        template_base: 0,
        template_size: 0,
        dtb_size: 0,
        management: false,
        smoke_payload: false,
        disks: [None; pauli_config::MAX_DISKS],
        vms: [Some(vm), None, None, None],
    };
    Ok((
        boot,
        BootImages {
            kernel,
            initrd: &[],
            dtb: &[],
        },
    ))
}
unsafe fn resources(
    external: Option<handoff::External<'_>>,
    layout: GuestBoardLayout,
) -> Result<(Payload, BootConfig, Slots)> {
    let (payload, boot, images) = match external {
        Some(input) => {
            // SAFETY: exclusive once-only boot writer; native DTB validation established
            // immutable image loans and an entirely disjoint guest RAM assignment.
            let (boot, images) = unsafe {
                super::external::stage(
                    input,
                    crate::guest::service(),
                    &mut *(&raw mut GUEST_DTB),
                    layout,
                )?
            };
            (Payload::Linux, boot, images)
        }
        None => {
            let (boot, images) = embedded()?;
            (Payload::Embedded, boot, [Some(images), None])
        }
    };
    // SAFETY: boot owns the linker arenas and each checked, disjoint guest grant.
    let slots = unsafe { Slots::bind(&boot, images, &mut *(&raw mut GUEST_ARENA))? };
    Ok((payload, boot, slots))
}
pub struct BootConsole;
impl Console for BootConsole {
    fn write(&self, bytes: &[u8]) -> core::fmt::Result {
        match SERIAL.get() {
            Some(uart) => Console::write(uart, bytes),
            None => Console::write(&sbi::FirmwareConsole, bytes),
        }
    }
}
pub fn flush_console() -> pauli_hal::device::Result<()> {
    SERIAL.get().map_or(Ok(()), Uart::flush_tx)
}
pub fn halt() -> ! {
    loop {
        core::hint::spin_loop();
    }
}
