//! Host device contracts, opaque block tickets and coalesced completion hints.
//! Ownership/Lock Ordering: VM devices -> native driver; never callback into a VM.
//! Invariants: submit copies writes; read collection copies owned bounce bytes only.

use core::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceError {
    InvalidResource,
    NotSupported,
    Busy,
    NotReady,
    Stale,
    Timeout,
    Fault,
    ReadOnly,
    Exhausted,
}
pub type Result<T> = core::result::Result<T, DeviceError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceResource {
    pub base: usize,
    pub size: usize,
    pub irq: Option<u32>,
}

/// Decoded guest device access; data only, never authority to dereference GPA.
#[derive(Clone, Copy, Debug)]
pub struct MmioAccess {
    pub gpa: usize,
    pub width: usize,
    pub write: Option<usize>,
}

pub struct CompletionEvent(AtomicBool);
impl CompletionEvent {
    pub const fn new() -> Self {
        Self(AtomicBool::new(false))
    }
    pub fn notify(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn take(&self) -> bool {
        self.0.load(Ordering::Acquire) && self.0.swap(false, Ordering::AcqRel)
    }
}
impl Default for CompletionEvent {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockOperation {
    Read,
    Write,
    Flush,
}
#[derive(Clone, Copy)]
pub struct BlockSubmission {
    pub operation: BlockOperation,
    pub sector: u64,
    pub bytes: usize,
    /// Absolute monotonic ticks. The dispatcher supplies the same clock to service.
    pub deadline: u64,
    pub event: Option<&'static CompletionEvent>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockToken {
    owner: usize,
    generation: u64,
    slot: u8,
}
impl BlockToken {
    /// Native driver ticket; never accept token contents from a guest.
    pub const fn new(owner: usize, generation: u64, slot: u8) -> Self {
        Self {
            owner,
            generation,
            slot,
        }
    }
    pub const fn owner(self) -> usize {
        self.owner
    }
    pub const fn generation(self) -> u64 {
        self.generation
    }
    pub const fn slot(self) -> usize {
        self.slot as usize
    }
}

pub trait HostCharDevice: Send + Sync {
    fn read_byte(&self) -> Option<u8>;
    fn write_byte(&self, byte: u8) -> Result<()>;
}
pub trait HostRtcDevice: Send + Sync {
    fn unix_ns(&self) -> Result<u64>;
}

pub trait HostBlockDevice: Send + Sync {
    fn sectors(&self) -> u64;
    fn writable(&self) -> bool;
    fn supports_flush(&self) -> bool;
    fn max_transfer_bytes(&self) -> usize;
    fn submit(&self, request: BlockSubmission, write: &[u8]) -> Result<BlockToken>;
    /// Nonblocking status only. Completion data stays private until collect/cancel.
    fn try_poll(&self, token: BlockToken) -> Result<Option<usize>>;
    /// Collect a terminal success/error and release the token. Busy/NotReady
    /// means the request remains owned and is NOT a retirement proof.
    fn collect(&self, token: BlockToken, out: &mut [u8]) -> Result<usize>;
    /// Pending cancellation quarantines DMA storage until used completion/reset ACK.
    fn cancel(&self, token: BlockToken) -> Result<()>;
    fn service(&self, now: u64) -> Result<usize>;
}

pub trait HostNetDevice: Send + Sync {
    fn mac_address(&self) -> [u8; 6];
    fn link_up(&self) -> bool;
    fn transmit(&self, frame: &[u8]) -> Result<()>;
    fn receive(&self, out: &mut [u8]) -> Result<Option<usize>>;
    /// Must observe native reset acknowledgement before clearing queue/bounce state.
    fn stop(&self) -> Result<()>;
    fn restart(&self) -> Result<()>;
}
