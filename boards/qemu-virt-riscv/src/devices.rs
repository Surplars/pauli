//! QEMU native devices and explicit logical VirtIO slot wiring.
//! Ownership/Lock Ordering: boot publishes each driver before enabling IRQ routes.
//! Invariants: QEMU coherent DMA allocations come only from the reserved host pool.
use crate::{
    fdt::{parsed, resource},
    irq::Root,
    loader::Loader,
};
use pauli_arch_riscv::host::irq::RiscvIrq;
use pauli_config::MAX_DISKS;
use pauli_core::{HvError, Result};
use pauli_hal::{
    device::{DeviceResource, HostBlockDevice, HostCharDevice, HostNetDevice},
    dma::{COHERENT, DmaRegion},
    fdt::Fdt,
    irq::{IrqHandler, IrqRoutes},
    log::Console,
    mem::PhysRange,
    sync::Once,
};
use pauli_virtio_common::{DEVICE_ID_BLOCK, DEVICE_ID_NET, MmioTransport};
pub type Uart = pauli_ns16550a::Ns16550a<RiscvIrq>;
pub type Rtc = pauli_goldfish_rtc::GoldfishRtc<RiscvIrq>;
type Block = pauli_virtio_blk::VirtioBlk<RiscvIrq>;
type Net = pauli_virtio_net::VirtioNet<RiscvIrq>;
const VIRTIO_SLOTS: usize = 8;
const UPLINK: usize = MAX_DISKS;
static UART: Once<Uart> = Once::new();
static RTC: Once<Rtc> = Once::new();
static BLOCK: [Once<Block>; MAX_DISKS] = [const { Once::new() }; MAX_DISKS];
static NET: Once<Net> = Once::new();
pub struct Devices {
    pub(crate) uart: &'static Uart,
    pub(crate) rtc: Option<&'static Rtc>,
}
struct Resources {
    uart: Option<(DeviceResource, u8, u8)>,
    rtc: Option<DeviceResource>,
    virtio: [Option<DeviceResource>; VIRTIO_SLOTS],
}
impl Resources {
    fn discover(fdt: Fdt<'_>, ram: PhysRange) -> Result<Self> {
        let mut devices = Self {
            uart: None,
            rtc: None,
            virtio: [None; VIRTIO_SLOTS],
        };
        let mut used = 0;
        for node in fdt.nodes() {
            let node = parsed(node)?;
            if node.depth() != 2 || !parsed(node.available())? {
                continue;
            }
            if parsed(node.compatible("ns16550a"))? {
                let shift = u8::try_from(parsed(node.u32("reg-shift"))?.unwrap_or(0))
                    .map_err(|_| HvError::InvalidDevice)?;
                let width = u8::try_from(parsed(node.u32("reg-io-width"))?.unwrap_or(1))
                    .map_err(|_| HvError::InvalidDevice)?;
                devices.uart = Some((resource(&node, ram)?, shift, width));
            } else if parsed(node.compatible("google,goldfish-rtc"))? {
                devices.rtc = Some(resource(&node, ram)?);
            } else if parsed(node.compatible("virtio,mmio"))? {
                *devices.virtio.get_mut(used).ok_or(HvError::InvalidDevice)? =
                    Some(resource(&node, ram)?);
                used += 1;
            }
        }
        // QEMU assigns bus.N to ascending MMIO windows, independent of node order.
        devices.virtio[..used].sort_unstable_by_key(|res| res.map(|r| r.base));
        Ok(devices)
    }
}
impl Devices {
    pub fn initialize(
        fdt: Fdt<'_>,
        ram: PhysRange,
        loader: Option<&mut Loader>,
        root: &Root,
    ) -> Result<(Self, IrqRoutes<6>)> {
        let res = Resources::discover(fdt, ram)?;
        let (uart_res, shift, width) = res.uart.ok_or(HvError::InvalidDevice)?;
        // SAFETY: persistent native UART node on the identity board bus; no other owner.
        let uart = publish(&UART, unsafe { Uart::new(uart_res, shift, width, 0) }?)?;
        let mut routes = IrqRoutes::new();
        install(&mut routes, root, uart_res, uart)?;
        let rtc = match res.rtc {
            // SAFETY: this is a retained physical Goldfish RTC range, never guest memory.
            Some(res) => Some(publish(&RTC, unsafe { Rtc::new(res) }?)?),
            None => None,
        };
        if let Some(loader) = loader {
            attach_virtio(loader, &res, root, &mut routes)?;
        }
        Ok((Self { uart, rtc }, routes))
    }
    pub fn block(&self, slot: u8) -> Result<&'static dyn HostBlockDevice> {
        BLOCK
            .get(slot as usize)
            .and_then(Once::get)
            .map(|driver| driver as &dyn HostBlockDevice)
            .ok_or(HvError::InvalidDevice)
    }
    pub fn net(&self) -> Result<&'static dyn HostNetDevice> {
        NET.get()
            .map(|driver| driver as &dyn HostNetDevice)
            .ok_or(HvError::InvalidDevice)
    }
    pub fn service(&self, now: u64) {
        self.uart.handle_irq(); // bounded polling also catches bytes before IRQ enable
        for device in BLOCK.iter().filter_map(Once::get) {
            let _ = device.service(now);
        }
    }
    pub fn input(&self) -> Option<u8> {
        self.uart.read_byte()
    }
    pub fn input_queued(&self) -> bool {
        self.uart.has_rx()
    }
}
fn attach_virtio(
    loader: &mut Loader,
    resources: &Resources,
    root: &Root,
    routes: &mut IrqRoutes<6>,
) -> Result<()> {
    for (slot, device) in BLOCK.iter().enumerate() {
        if loader.config.disks[slot].is_none() {
            continue;
        }
        let res = transport(resources, slot, DEVICE_ID_BLOCK)?;
        let dma = dma(loader, pauli_virtio_blk::DMA_BYTES)?;
        // SAFETY: verified physical transport and a new, driver-owned, coherent host DMA range.
        let driver = publish(device, unsafe { Block::new(res, dma) }?)?;
        install(routes, root, res, driver)?;
    }
    if loader.config.vms.iter().flatten().any(|vm| vm.net_uplink) {
        let res = transport(resources, UPLINK, DEVICE_ID_NET)?;
        let dma = dma(loader, pauli_virtio_net::DMA_BYTES)?;
        // SAFETY: one exclusive native NIC and an immutable reservation for its DMA storage.
        let driver = publish(&NET, unsafe { Net::new(res, dma) }?)?;
        install(routes, root, res, driver)?;
    }
    Ok(())
}
fn dma(loader: &mut Loader, size: usize) -> Result<DmaRegion> {
    let scratch = loader.take_bytes(size, 4096)?;
    let address = scratch.as_ptr() as u64;
    // SAFETY: QEMU virt's identity DMA is coherent; slice is exclusive and retained.
    Ok(unsafe { DmaRegion::new(scratch, address, &COHERENT) }?)
}
fn transport(resources: &Resources, slot: usize, id: u32) -> Result<DeviceResource> {
    let res = resources.virtio[slot].ok_or(HvError::InvalidDevice)?;
    // SAFETY: this exact resource was discovered and checked against the host RAM range.
    unsafe { MmioTransport::new(res)? }.validate_device(id)?;
    Ok(res)
}
fn install(
    routes: &mut IrqRoutes<6>,
    root: &Root,
    res: DeviceResource,
    driver: &'static dyn IrqHandler,
) -> Result<()> {
    let irq = res.irq.ok_or(HvError::InvalidDevice)?;
    routes.register(irq, driver)?;
    root.enable(irq)
}
fn publish<T>(place: &'static Once<T>, value: T) -> Result<&'static T> {
    place.set(value).map_err(|_| HvError::Busy)?;
    place.get().ok_or(HvError::InvalidState)
}
pub struct BootConsole;
impl Console for BootConsole {
    fn write(&self, bytes: &[u8]) -> core::fmt::Result {
        match UART.get() {
            Some(uart) => Console::write(uart, bytes),
            None => Console::write(&pauli_arch_riscv::host::sbi::FirmwareConsole, bytes),
        }
    }
}
