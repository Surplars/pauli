//! Modern VirtIO-net: one RX/TX split queue pair, no offload/control/merge.
//! One serialized VM device owner owns all state. Only service() performs I/O, through
//! copied frames; guest descriptors and pointers never reach native DMA.
//! Ownership/Lock Ordering: exclusive device owner -> backend; no internal locks.
//! Invariants: reset discards rings/IRQ/bounce state and retains the assigned MAC.
//! Lifecycle: reset -> negotiate -> ready -> service; reset/fault leaves no pending packet.
use crate::memory::GuestMemory;
use crate::virtio::*;
use core::sync::atomic::{Ordering, fence};

pub const QUEUE_MAX: u16 = 64;
pub const HEADER: usize = 12;
pub const FRAME_MAX: usize = 1514;
const FEATURE_MAC: u64 = 1 << 5;
const FEATURE_STATUS: u64 = 1 << 16;
const FEATURES: u64 = VERSION_1 | FEATURE_MAC | FEATURE_STATUS;
const OK: u32 = DRIVER_OK | FEATURES_OK;
const BROKEN: u32 = NEEDS_RESET;

#[derive(Debug, PartialEq, Eq)]
pub enum NetError {
    Busy,
    Fault,
}
pub trait NetBackend {
    /// The frontend validates 14..=FRAME_MAX bytes once before calling a
    /// backend. Implementations copy/consume now and never retain this slice.
    fn transmit(&mut self, frame: &[u8]) -> Result<(), NetError>;
    fn receive(&mut self, frame: &mut [u8]) -> Result<Option<usize>, NetError>;
}
pub struct VirtioNet {
    mac: [u8; 6],
    queues: [Queue; 2],
    queue_sel: u32,
    feature_sel: u32,
    driver_sel: u32,
    driver_features: u64,
    unsupported: bool,
    status: u32,
    interrupts: u32,
    bounce: [u8; HEADER + FRAME_MAX],
    pub tx_packets: u64,
    pub rx_packets: u64,
    link_up: bool,
    generation: u32,
}
impl VirtioNet {
    pub const fn new(mac: [u8; 6]) -> Self {
        Self {
            mac,
            queues: [Queue::new(); 2],
            queue_sel: 0,
            feature_sel: 0,
            driver_sel: 0,
            driver_features: 0,
            unsupported: false,
            status: 0,
            interrupts: 0,
            bounce: [0; HEADER + FRAME_MAX],
            tx_packets: 0,
            rx_packets: 0,
            link_up: false,
            generation: 0,
        }
    }
    pub fn set_link(&mut self, up: bool) {
        if self.link_up == up {
            return;
        }
        self.link_up = up;
        self.generation = self.generation.wrapping_add(1);
        if self.status & DRIVER_OK != 0 && self.driver_features & FEATURE_STATUS != 0 {
            self.interrupts |= IRQ_CONFIG;
        }
    }
    pub fn irq_pending(&self) -> bool {
        self.interrupts != 0
    }
    fn broken(&mut self) {
        self.status |= BROKEN;
        self.bounce.fill(0);
        if self.status & DRIVER_OK != 0 {
            self.interrupts |= IRQ_CONFIG;
        }
    }
}

mod queue;
mod registers;
mod service;
use queue::Queue;

fn low(old: u64, value: u32) -> u64 {
    (old & !0xffff_ffff) | u64::from(value)
}
fn high(old: u64, value: u32) -> u64 {
    (u64::from(value) << 32) | (old & 0xffff_ffff)
}
fn overlap(a: u64, al: usize, b: u64, bl: usize) -> bool {
    al != 0 && bl != 0 && a < b + bl as u64 && b < a + al as u64
}
