#![no_std]
//! Native modern VirtIO Ethernet with VM-independent, driver-owned DMA pools.
//! Ownership/Lock Ordering: exclusive uplink assignment -> short native queue lock.
//! Invariants: copied frames only; stop/reset ACK precedes queue/buffer/IRQ reuse.
//! Lifecycle: stopped -> configuring -> running -> stopped/failed; restart resets both queues.

use core::sync::atomic::{Ordering, fence};
use pauli_hal::{
    device::{DeviceError, DeviceResource, HostNetDevice, Result},
    dma::{Direction, DmaRegion},
    irq::IrqHandler,
    sync::{IrqSpinLock, LocalIrq},
};
use pauli_virtio_common::{
    DEVICE_ID_NET, FeatureSet, MmioTransport, VirtqAvail, VirtqDesc, VirtqUsed, desc_flags, status,
};
mod frame;
mod io;
mod queue;
use frame::{FRAME, HEADER};

const N: usize = 8;
const BUFFER: usize = 2048;
const QUEUE_BYTES: usize = 4096;
const AVAIL: usize = N * 16;
const USED: usize = 256;
const DATA: usize = 2 * QUEUE_BYTES;
const RX: usize = 0;
const TX: usize = 1;
const FEATURE_MAC: u32 = 1 << 5;
const FEATURE_LINK: u32 = 1 << 16;
const CONFIG_LINK: u32 = 1 << 16;
pub const DMA_BYTES: usize = DATA + 2 * N * BUFFER;
const REQUESTED: FeatureSet = FeatureSet {
    lo: FEATURE_MAC | FEATURE_LINK,
    hi: 0,
};

struct State {
    rx_used: u16,
    rx_available: u16,
    tx_used: u16,
    tx_available: u16,
    tx_busy: [bool; N],
    running: bool,
    resetting: bool,
    failed: bool,
}
impl State {
    const fn new(running: bool) -> Self {
        Self {
            rx_used: 0,
            rx_available: N as u16,
            tx_used: 0,
            tx_available: 0,
            tx_busy: [false; N],
            running,
            resetting: false,
            failed: false,
        }
    }
    fn ready(&self) -> Result<()> {
        if self.failed {
            Err(DeviceError::Fault)
        } else if !self.running {
            Err(DeviceError::NotReady)
        } else {
            Ok(())
        }
    }
}
pub struct VirtioNet<I: LocalIrq> {
    transport: MmioTransport,
    dma: DmaRegion,
    mac: [u8; 6],
    features: u32,
    state: IrqSpinLock<State, I>,
}
impl<I: LocalIrq> VirtioNet<I> {
    /// # Safety
    /// Native mapped VirtIO-MMIO and exclusive pinned host DMA region, with
    /// correct board cache operations. Never pass guest RAM or a caller packet.
    pub unsafe fn new(resource: DeviceResource, dma: DmaRegion) -> Result<Self> {
        dma.check_layout(DMA_BYTES, 4096)?;
        // SAFETY: caller supplies a persistent native register mapping.
        let transport = unsafe { MmioTransport::new(resource)? };
        transport.validate_device(DEVICE_ID_NET)?;
        transport.reset_and_wait()?;
        transport.set_status_bits(status::ACKNOWLEDGE | status::DRIVER);
        let features = transport.negotiate_features(REQUESTED)?.lo;
        if features & FEATURE_MAC == 0 {
            return Err(DeviceError::NotSupported);
        }
        let config = transport.read_config64(0)?.to_le_bytes();
        let mut mac = [0; 6];
        mac.copy_from_slice(&config[..6]);
        let driver = Self {
            transport,
            dma,
            mac,
            features,
            state: IrqSpinLock::new(State::new(false)),
        };
        driver.configure()?;
        *driver.state.lock() = State::new(true);
        Ok(driver)
    }
    fn data(queue: usize, id: usize) -> usize {
        DATA + (queue * N + id) * BUFFER
    }
    fn desc(&self, queue: usize) -> *mut VirtqDesc {
        // SAFETY: fixed queue index 0/1 and aligned layout validated by new.
        unsafe { self.dma.pointer(queue * QUEUE_BYTES).cast() }
    }
    fn available(&self, queue: usize) -> *mut VirtqAvail<N> {
        // SAFETY: fixed queue index and available range lie inside DMA_BYTES.
        unsafe { self.dma.pointer(queue * QUEUE_BYTES + AVAIL).cast() }
    }
    fn used(&self, queue: usize) -> *mut VirtqUsed<N> {
        // SAFETY: fixed queue index and used range lie inside DMA_BYTES.
        unsafe { self.dma.pointer(queue * QUEUE_BYTES + USED).cast() }
    }
}
impl<I: LocalIrq> HostNetDevice for VirtioNet<I> {
    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }
    fn link_up(&self) -> bool {
        self.features & FEATURE_LINK == 0
            || self
                .transport
                .read_config32(4)
                .is_ok_and(|value| value & CONFIG_LINK != 0)
    }
    fn transmit(&self, frame: &[u8]) -> Result<()> {
        self.send(frame)
    }
    fn receive(&self, out: &mut [u8]) -> Result<Option<usize>> {
        self.recv(out)
    }
    fn stop(&self) -> Result<()> {
        self.reset(false)
    }
    fn restart(&self) -> Result<()> {
        self.reset(true)
    }
}
impl<I: LocalIrq> IrqHandler for VirtioNet<I> {
    fn handle_irq(&self) -> bool {
        self.transport.ack_interrupts() != 0
    }
}
