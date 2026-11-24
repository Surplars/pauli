//! VirtIO-MMIO v2 block device with one bounded split virtqueue.
//!
//! The VM dispatcher exclusively owns this state. MMIO only changes registers;
//! `service` runs after VM exit, with host interrupts enabled. Guest memory must
//! remain assigned throughout service. A descriptor/header snapshot survives
//! pending I/O, but no guest pointer or buffer borrow escapes a call. Backend
//! DMA uses backend-owned storage only. This protocol has no ISA, board,
//! allocator or kernel API.
//! Ownership/Lock Ordering: serialized device owner -> backend, no internal locks.
//! Invariants: live buffer is always Some; occupied slots own captured requests.
//! Lifecycle: reset -> drain -> negotiated/ready -> active -> reset/fault -> drain.
use crate::virtio::*;
use core::sync::atomic::{Ordering, fence};
use core::task::Poll;
mod notification;
mod request;
pub use notification::NotificationStats;
use request::Request;

pub const MAX_QUEUE_SIZE: u16 = 64;
pub const MAX_SEGMENTS: usize = 16;
pub const MAX_SEGMENT_SIZE: usize = 4096;
pub const MAX_TRANSFER: usize = MAX_SEGMENTS * MAX_SEGMENT_SIZE;
pub const MAX_INFLIGHT: usize = 4;
pub const MAX_SERVICE_REQUESTS: usize = 4;
const FEATURES: u64 = (1 << 32) | (1 << 1) | (1 << 2) | (1 << 6);
const READ_ONLY: u64 = 1 << 5;
const FLUSH: u64 = 1 << 9;
const EVENT_IDX: u64 = 1 << 29;
// Bounds are part of the negotiated contract, not undocumented I/O failures.
const REQUIRED_FEATURES: u64 = (1 << 32) | (1 << 1) | (1 << 2);
const NEXT: u16 = 1;
const WRITE: u16 = 2;

/// Guest-physical RAM only. The adapter checks ownership and the requested
/// access permission over the entire range, including every crossed page.
/// Firmware, host memory, other VMs and MMIO must never pass `check`.
pub use crate::memory::GuestMemory;

/// One VM's nonblocking storage adapter. Each trusted slot identifies one
/// operation; pending calls retry its same operation/sector/length. Slots are
/// reused only after completion or reset drain. Never retain the caller's
/// buffer or resubmit an active operation. Read-only backing may be shared, but
/// request handles remain adapter-local. Successful flush commits preceding
/// completed writes. Submit/poll work must be bounded, never a completion wait.
pub trait BlockBackend {
    fn sectors(&self) -> u64;
    /// Trusted adapter bound: a nonzero sector multiple, at most MAX_TRANSFER.
    /// The frontend also caps each operation to its owned staging buffer.
    fn max_transfer_bytes(&self) -> usize {
        MAX_SEGMENT_SIZE
    }
    fn read(&mut self, slot: usize, sector: u64, out: &mut [u8]) -> Poll<bool>;
    /// Optional scoped CPU-owned read view. None means unsupported, with no
    /// work started. Pending never calls/retains the consumer. A successful
    /// completion calls it exactly once with the requested bytes, before slot
    /// reuse. This is host storage, not permission for DMA into guest memory.
    fn read_with(
        &mut self,
        _slot: usize,
        _sector: u64,
        _bytes: usize,
        _consume: &mut dyn FnMut(&[u8]) -> bool,
    ) -> Option<Poll<bool>> {
        None
    }
    fn write(&mut self, _slot: usize, _sector: u64, _data: &[u8]) -> Poll<bool> {
        Poll::Ready(false)
    }
    fn flush(&mut self, _slot: usize) -> Poll<bool> {
        Poll::Ready(false)
    }
    /// Finish/discard an old operation after frontend reset or a broken queue.
    /// Do not start new I/O or publish old results into guest RAM. A synchronous
    /// backend has nothing to drain; an asynchronous adapter overrides this.
    fn drain(&mut self, _slot: usize, _scratch: &mut [u8]) -> Poll<()> {
        Poll::Ready(())
    }
}

#[derive(Clone, Copy)]
struct Segment {
    address: u64,
    len: usize,
    write: bool,
}
const EMPTY_SEGMENT: Segment = Segment {
    address: 0,
    len: 0,
    write: false,
};

struct Chain {
    segments: [Segment; MAX_QUEUE_SIZE as usize],
    count: usize,
    readable: usize,
    writable: usize,
}

mod chain;
mod queue;
mod registers;
mod service;

pub struct VirtioBlock<B = [u8; MAX_SEGMENT_SIZE * MAX_INFLIGHT]> {
    sectors: u64,
    writable: bool,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    unsupported_features: bool,
    queue_sel: u32,
    queue_num: u16,
    descriptors: u64,
    available: u64,
    used: u64,
    ready: bool,
    notified: bool,
    // True only while the published avail_event still covers last_available.
    // Consumption/reset invalidates it, including a complete u16 index wrap.
    available_armed: bool,
    last_available: u16,
    used_index: u16,
    status: u32,
    interrupts: u32,
    pending: [Option<Request>; MAX_INFLIGHT],
    inflight: usize,
    next_poll: usize,
    barrier: Option<usize>,
    draining: bool,
    // Option is used only to move storage through a register reset. A live
    // frontend always owns Some(buffer), including while native I/O is pending.
    buffer: Option<B>,
    pub completed: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub flushes: u64,
    pub peak_inflight: usize,
    pub completion_batches: u64,
    pub largest_batch: usize,
    pub irq_edges: u64,
    pub notifications: NotificationStats,
}

impl VirtioBlock {
    /// Read-only remains the default. Capacity matches the assigned backend.
    pub const fn new(sectors: u64) -> Self {
        Self::with_access(sectors, false)
    }

    /// A writable assignment requires a backend with real write AND flush.
    /// No configurable writeback cache is exposed. With FLUSH unnegotiated,
    /// each write is flushed before completion (VirtIO stable-write contract).
    pub const fn with_access(sectors: u64, writable: bool) -> Self {
        Self::init(sectors, writable, [0; MAX_SEGMENT_SIZE * MAX_INFLIGHT])
    }
}

impl<B> VirtioBlock<B> {
    const fn init(sectors: u64, writable: bool, buffer: B) -> Self {
        Self {
            sectors,
            writable,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            unsupported_features: false,
            queue_sel: 0,
            queue_num: 0,
            descriptors: 0,
            available: 0,
            used: 0,
            ready: false,
            notified: false,
            available_armed: false,
            last_available: 0,
            used_index: 0,
            status: 0,
            interrupts: 0,
            pending: [const { None }; MAX_INFLIGHT],
            inflight: 0,
            next_poll: 0,
            barrier: None,
            draining: true,
            buffer: Some(buffer),
            completed: 0,
            read_bytes: 0,
            write_bytes: 0,
            flushes: 0,
            peak_inflight: 0,
            completion_batches: 0,
            largest_batch: 0,
            irq_edges: 0,
            notifications: NotificationStats {
                kicks: 0,
                checks: 0,
                suppressed: 0,
                rearms: 0,
                reuses: 0,
                recheck_races: 0,
            },
        }
    }
}

impl<B: AsMut<[u8]>> VirtioBlock<B> {
    /// Supply stable per-frontend staging storage without a large stack object
    /// or allocator dependency. Four equal, sector-aligned slices are private
    /// to the request slots. Storage survives pending I/O and guest resets.
    pub fn with_buffer(sectors: u64, writable: bool, mut buffer: B) -> Self {
        let len = buffer.as_mut().len();
        // Trusted owner storage, not guest input: a malformed allocation is a
        // programming error that cannot satisfy private, sector-aligned slots.
        assert!(len != 0 && len % (512 * MAX_INFLIGHT) == 0 && len <= MAX_TRANSFER * MAX_INFLIGHT);
        Self::init(sectors, writable, buffer)
    }

    /// The owner may release its buffer lease only after all vCPUs are joined
    /// and backend operations drained/cancelled or detached from this staging
    /// storage. No service may follow release.
    pub fn buffer_mut(&mut self) -> &mut B {
        self.buffer.as_mut().unwrap()
    }

    fn features(&self) -> u64 {
        FEATURES | EVENT_IDX | if self.writable { FLUSH } else { READ_ONLY }
    }

    pub fn irq_pending(&self) -> bool {
        self.interrupts != 0
    }

    /// Frontend queue work only. An asynchronous adapter must ALSO service a
    /// retained native handle/admission attempt after guest reset or failure,
    /// even when this returns false. No guest pointers are inspected here.
    pub fn needs_service(&self) -> bool {
        self.ready
            && (self.notified || self.inflight != 0)
            && self.status & (DRIVER_OK | FEATURES_OK) == (DRIVER_OK | FEATURES_OK)
            && self.status & (FAILED | NEEDS_RESET) == 0
    }
}

fn checked(memory: &impl GuestMemory, address: u64, len: usize, write: bool) -> bool {
    // A zero-length descriptor performs no access; its address need not denote
    // assigned RAM. Stream traversal skips it without forming a host pointer.
    len == 0 || (address.checked_add(len as u64).is_some() && memory.check(address, len, write))
}
fn overlaps(a: u64, a_len: usize, b: u64, b_len: usize) -> bool {
    a_len != 0 && b_len != 0 && a < b + b_len as u64 && b < a + a_len as u64
}
fn set_low(address: u64, value: u32) -> u64 {
    (address & 0xffff_ffff_0000_0000) | u64::from(value)
}
fn set_high(address: u64, value: u32) -> u64 {
    (address & 0xffff_ffff) | (u64::from(value) << 32)
}
