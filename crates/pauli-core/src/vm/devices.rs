//! VM-owned I/O and short IRQ bank, selected through immutable MMIO routes.
//! Ownership/Lock Ordering: I/O owner -> IRQ bank -> link/native driver when needed.
//! Invariants: never hold the I/O lock for file transfers; no physical output here.
use super::irq_bank::IrqBank;
use crate::{
    HvError, Result,
    arch::{GuestIo, IrqRegion, RegisterAccess, VirtualIrqChip},
    backends::{block::Block, console::Console, net::Net, rtc::Rtc},
    board::Board,
    bus::{DeviceId, MmioBus},
    memory::{Arena, GuestRam},
    trace::Tracer,
};
use core::sync::atomic::{AtomicUsize, Ordering};
use pauli_config::{BootConfig, GuestBoardLayout, VmConfig};
use pauli_devices::net_link::Link;
use pauli_hal::{
    device::{CompletionEvent, MmioAccess},
    sync::SpinLock,
};
/// Stable driver notification targets. Boot VMs may share a read-only disk;
/// managed disks instead have one exclusive owner, whose VM ID need not be small.
pub(crate) struct DeviceLinks {
    pub peer: SpinLock<Link>,
    boot: [CompletionEvent; pauli_config::MAX_BOOT_VMS],
    managed: [CompletionEvent; pauli_config::MAX_DISKS],
}
impl DeviceLinks {
    pub(crate) const fn new() -> Self {
        Self {
            peer: SpinLock::new(Link::new()),
            boot: [const { CompletionEvent::new() }; pauli_config::MAX_BOOT_VMS],
            managed: [const { CompletionEvent::new() }; pauli_config::MAX_DISKS],
        }
    }
}
pub struct IoState {
    pub ram: GuestRam,
    pub console: Console,
    pub block: Option<Block>,
    pub net: Option<Net>,
    pub rtc: Rtc,
    levels: [bool; 4],
}
impl IoState {
    /// Block and net retain their polling bound even when their queues look empty:
    /// completions and incoming frames may originate outside this VM.
    pub(crate) fn needs_fast_idle_service(&self) -> bool {
        self.block.is_some() || self.net.is_some() || self.console.needs_fast_idle_service()
    }
}
pub struct VmDevices<I: VirtualIrqChip> {
    pub io: SpinLock<IoState>,
    pub(crate) irqs: SpinLock<IrqBank<I>>,
    /// Guest vCPU lanes whose owner must resample the IRQ chip.
    pub(crate) wake: AtomicUsize,
    bus: MmioBus,
    layout: GuestBoardLayout,
    pub(super) lanes: usize,
}
impl<I: VirtualIrqChip> VmDevices<I> {
    pub(crate) fn create<B: Board>(
        board: &B,
        boot: &BootConfig,
        config: &VmConfig,
        slot: usize,
        epoch: u32,
        ram: GuestRam,
        arena: &mut Arena,
        links: &'static DeviceLinks,
        now: u64,
    ) -> Result<Self> {
        let hz = board.info().timebase_hz;
        let layout = board.layout();
        let block = bind_block(board, boot, config, slot, links, hz, arena)?;
        let bus = create_bus(layout, config, &ram)?;
        let native = if config.net_uplink {
            Some(board.resolve_uplink_net()?)
        } else {
            None
        };
        // No physical RTC means a declared epoch-zero wall clock, not a guessed
        // date. The guest may set its own UTC; QEMU/real RTC errors are propagated.
        let unix = match board.rtc() {
            Some(rtc) => rtc.unix_ns()?,
            None => 0,
        };
        let chip = I::create(config.vcpus, config.aia)?;
        // Binding the peer link is the final fallible step: no failed VM can
        // leave behind an endpoint that a later peer would see as active.
        let net = Net::bind(slot, epoch, config, &links.peer, native)?;
        Ok(Self {
            io: SpinLock::new(IoState {
                ram,
                console: Console::new((hz / 1000).max(1)),
                block,
                net,
                rtc: Rtc::new(unix, now, hz),
                levels: [false; 4],
            }),
            irqs: SpinLock::new(IrqBank::new(chip, config)),
            wake: AtomicUsize::new(0),
            bus,
            layout,
            lanes: usize::MAX >> (usize::BITS - u32::from(config.vcpus)),
        })
    }
    pub(super) fn replace_chip(&self, io: &mut IoState, chip: I, config: &VmConfig) {
        *self.irqs.lock() = IrqBank::new(chip, config);
        io.levels = [false; 4];
        self.wake.store(0, Ordering::Relaxed);
    }
    fn levels(&self, io: &mut IoState, exclude: usize) {
        let levels = [
            io.console.uart.irq_pending(),
            io.block.as_ref().is_some_and(|b| b.device.irq_pending()),
            io.net.as_ref().is_some_and(|n| n.device.irq_pending()),
            io.rtc.device.irq_pending(),
        ];
        if levels == io.levels {
            return;
        }
        let sources = [
            self.layout.uart_irq,
            self.layout.block_irq,
            self.layout.net_irq,
            self.layout.rtc_irq,
        ];
        self.change_irqs(exclude, |chip| {
            for (i, source) in sources.into_iter().enumerate() {
                if levels[i] != io.levels[i] {
                    chip.set_level(source, levels[i]);
                }
            }
        });
        io.levels = levels;
    }
    /// A resident interrupt file can drain its inbox between producer calls.
    /// Compare the two sides of this change under one lock, never a cached sample.
    fn change_irqs<T>(&self, exclude: usize, change: impl FnOnce(&mut IrqBank<I>) -> T) -> T {
        let recipients = self.lanes & !exclude;
        let mut chip = self.irqs.lock();
        let before = Self::pending(&chip, recipients);
        #[cfg(feature = "selftest")]
        {
            chip.rx.awaiting = false;
        }
        let value = change(&mut chip);
        let after = Self::pending(&chip, recipients);
        #[cfg(feature = "selftest")]
        {
            // Only an RX publication that made the probe's sole lane eligible
            // can arm timing. Other sources, masked RX and unchanged lines cannot.
            chip.rx.awaiting &= before == 0 && after == 1;
        }
        let changed = before ^ after;
        if changed != 0 {
            let _previous = self.wake.fetch_or(changed, Ordering::Release);
            #[cfg(feature = "selftest")]
            {
                chip.rx.solo_wake = chip.rx.awaiting && _previous == 0;
            }
        }
        value
    }
    fn pending(chip: &I, mut lanes: usize) -> usize {
        // Configured lanes are dense except for the issuer. Walk in index order
        // instead of paying for a software trailing-zero lookup on RV64IMAC.
        let (mut mask, mut cpu) = (0, 0);
        while lanes != 0 {
            if lanes & 1 != 0 {
                mask |= usize::from(chip.pending(cpu)) << cpu;
            }
            lanes >>= 1;
            cpu += 1;
        }
        mask
    }
    pub(crate) fn receive_console(&self, byte: u8, clock: impl Fn() -> u64) -> bool {
        let mut io = self.io.lock();
        let IoState {
            console, levels, ..
        } = &mut *io;
        let uart = &mut console.uart;
        uart.advance(clock());
        // A new byte restarts the FIFO timeout. Latch an already-expired line
        // first; even a rejected byte can then assert a line-status overrun.
        let before = uart.irq_pending();
        let accepted = uart.receive(byte);
        let after = uart.irq_pending();
        let cached = &mut levels[0]; // UART is the first source in levels().
        if *cached != before || before != after {
            self.change_irqs(0, |chip| {
                for level in [before, after] {
                    if *cached != level {
                        // Timestamp the accepted byte's own rising edge under the
                        // chip lock, not a timeout or overrun from an earlier byte.
                        #[cfg(feature = "selftest")]
                        let rise = (accepted && !before && level).then(&clock);
                        chip.set_level(self.layout.uart_irq, level);
                        #[cfg(feature = "selftest")]
                        if let Some(ticks) = rise {
                            // Publish the sampled value before the chip lock releases the edge.
                            chip.rx.record_rise(byte, ticks);
                        }
                        *cached = level;
                    }
                }
            });
        }
        accepted
    }
    pub fn service(&self, clock: impl FnOnce() -> u64, link: &SpinLock<Link>) -> Result<()> {
        let mut io = self.io.lock();
        // All device time samples are ordered by this VM's I/O lock. A poll
        // queued behind another hart must not rewind its UART or RTC.
        let now = clock();
        io.console.uart.advance(now);
        io.rtc.advance(now);
        let IoState {
            ram, block, net, ..
        } = &mut *io;
        if let Some(block) = block {
            block.service(ram, now);
        }
        if let Some(net) = net {
            net.service(ram, link)?;
        }
        self.levels(&mut io, 0);
        Ok(())
    }
    pub fn access(
        &self,
        access: MmioAccess,
        clock: impl FnOnce() -> u64,
        link: &SpinLock<Link>,
    ) -> Option<usize> {
        self.access_from(None, access, clock, link)
    }
    pub(crate) fn access_from(
        &self,
        origin: Option<u8>,
        access: MmioAccess,
        clock: impl FnOnce() -> u64,
        link: &SpinLock<Link>,
    ) -> Option<usize> {
        self.access_io(origin, access, clock, link, None).0
    }
    fn access_io(
        &self,
        origin: Option<u8>,
        access: MmioAccess,
        clock: impl FnOnce() -> u64,
        link: &SpinLock<Link>,
        service: Option<&mut MonitorDeadline>,
    ) -> (Option<usize>, bool, bool) {
        // Only the issuer is certain to sample the chip before its next entry.
        let exclude = origin.map_or(0, |cpu| 1usize << cpu);
        let Some((device, offset)) = self.bus.resolve(access) else {
            return (None, false, false);
        };
        if let DeviceId::Irq(region) = device {
            if access.width != 4 {
                return (None, false, false);
            }
            // A claim read can also lower another context's external interrupt.
            return (
                self.change_irqs(exclude, |chip| chip.access(region, offset, access.write)),
                false,
                false,
            );
        }
        let mut io = self.io.lock();
        let now = clock();
        io.console.uart.advance(now);
        io.rtc.advance(now);
        self.levels(&mut io, exclude); // Latch timeout before destructive RBR/IIR reads.
        let mut console_tx = false;
        let mut console_full = false;
        let value = match device {
            DeviceId::Uart if access.width == 1 || access.width == 4 => {
                let value = io.console.access(offset, access.write);
                if access.write.is_some() {
                    console_tx = true;
                    console_full = io.console.is_full();
                }
                value
            }
            DeviceId::Block if access.width == 4 => io.block.as_mut().and_then(|b| b.access(offset, access.write)),
            DeviceId::Net => io
                .net
                .as_mut()
                .and_then(|n| n.access(offset, access.width, access.write, link)),
            DeviceId::Rtc if access.width == 4 => io.rtc.access(offset, access.write),
            _ => None,
        };
        if let Some(service) = service {
            match device {
                // RBR reads restart a sub-trigger FIFO timeout. Writes can
                // reconfigure its IRQ/trigger or append bytes to virtual TX.
                DeviceId::Uart if access.write.is_some() || offset == 0 => {
                    service.observe_console(&io.console, true);
                }
                DeviceId::Rtc if access.write.is_some() => service.observe_rtc(&io.rtc),
                _ => {}
            }
        }
        self.levels(&mut io, exclude);
        (value, console_tx, console_full)
    }
    pub fn stop(&self, now: u64, link: &SpinLock<Link>) -> Result<bool> {
        let mut io = self.io.lock();
        if let Some(block) = &mut io.block {
            if !block.stop(now)? {
                return Ok(false);
            }
        }
        if let Some(net) = &mut io.net {
            net.stop(link)?;
        }
        self.irqs.lock().quiesce()?;
        Ok(true)
    }
}
fn bind_block<B: Board>(
    board: &B,
    boot: &BootConfig,
    config: &VmConfig,
    slot: usize,
    links: &'static DeviceLinks,
    hz: u64,
    arena: &mut Arena,
) -> Result<Option<Block>> {
    let Some(index) = config.disk else {
        return Ok(None);
    };
    let disk = boot
        .disks
        .get(index as usize)
        .and_then(Option::as_ref)
        .ok_or(HvError::InvalidConfig)?;
    let event = if disk.managed {
        &links.managed[index as usize] // The descriptor just validated this index.
    } else {
        links.boot.get(slot).ok_or(HvError::InvalidConfig)?
    };
    Ok(Some(Block::bind(
        event,
        board.resolve_block(index)?,
        disk.sectors,
        config.writable,
        hz,
        arena,
    )?))
}
fn create_bus(layout: GuestBoardLayout, config: &VmConfig, ram: &GuestRam) -> Result<MmioBus> {
    let sources = [
        layout.uart_irq,
        layout.block_irq,
        layout.net_irq,
        layout.rtc_irq,
    ];
    for (index, source) in sources.iter().enumerate() {
        if !(1..32).contains(source) || sources[..index].contains(source) {
            return Err(HvError::InvalidDevice);
        }
    }
    let mut bus = MmioBus::new();
    bus.register(layout.uart, DeviceId::Uart)?;
    bus.register(layout.rtc, DeviceId::Rtc)?;
    if config.disk.is_some() {
        bus.register(layout.block, DeviceId::Block)?;
    }
    if config.net_peer.is_some() || config.net_uplink {
        bus.register(layout.net, DeviceId::Net)?;
    }
    if config.aia {
        bus.register(layout.aplic, DeviceId::Irq(IrqRegion::Wired))?;
        let files = layout
            .imsic_files(config.vcpus)
            .ok_or(HvError::InvalidDevice)?;
        bus.register(files, DeviceId::Irq(IrqRegion::Message))?;
    } else {
        let region = layout
            .plic_contexts(config.vcpus)
            .ok_or(HvError::InvalidDevice)?;
        bus.register(region, DeviceId::Irq(IrqRegion::Legacy))?;
    }
    bus.validate_ram(ram.guest_base(), ram.len())?;
    Ok(bus)
}
/// Absolute upper bound for a single extended management-hart grant.
pub(crate) struct MonitorDeadline {
    pub at: u64,
    pub flush_at: u64,
}
impl MonitorDeadline {
    pub fn observe_console(&mut self, console: &Console, running: bool) {
        if console.is_full() {
            self.at = 0;
        } else if console.has_output() {
            self.at = self.at.min(self.flush_at);
        }
        if running && let Some(at) = console.uart.rx_timeout_deadline() {
            self.at = self.at.min(at);
        }
    }
    pub fn observe_rtc(&mut self, rtc: &Rtc) {
        if let Some(at) = rtc.irq_deadline() {
            self.at = self.at.min(at);
        }
    }
}
pub(crate) struct DeviceIo<'a, I: VirtualIrqChip, R: Tracer> {
    pub devices: &'a VmDevices<I>,
    pub origin: u8,
    pub link: &'a SpinLock<Link>,
    pub now: fn() -> u64,
    pub trace: &'a mut R,
    pub console_tx: bool,
    pub console_full: bool,
    pub service: Option<&'a mut MonitorDeadline>,
}
impl<I: VirtualIrqChip, R: Tracer> GuestIo for DeviceIo<'_, I, R> {
    fn mmio(&mut self, access: MmioAccess) -> Option<usize> {
        self.trace.on_mmio(access.gpa, access.write.is_some());
        let (value, console_tx, console_full) = self.devices.access_io(
            Some(self.origin),
            access,
            self.now,
            self.link,
            self.service.as_deref_mut(),
        );
        self.console_tx |= console_tx;
        self.console_full |= console_full;
        value
    }
    fn register(&mut self, cpu: u8, access: RegisterAccess) -> Option<usize> {
        self.devices
            .change_irqs(1usize << self.origin, |chip| chip.register(cpu, access))
    }
}
