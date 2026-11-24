//! Isolated point-to-point Ethernet transport. Host-owned frames only: no guest
//! addresses, DMA, interrupts or callbacks. The board serializes each Link.
//! Ownership/Lock Ordering: VM devices -> externally serialized link; no locks here.
//! Invariants: endpoint epochs cannot consume or publish a replacement VM's frames.
//! Lifecycle: unbound -> bound -> active -> retired; reset clears both directions.
use crate::virtio_net::{FRAME_MAX, NetBackend, NetError};

pub const PORTS: usize = 4;
pub const DEPTH: usize = 4;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Endpoint {
    slot: usize,
    epoch: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Bound,
    Stale,
}
#[derive(Clone, Copy)]
struct Frame {
    len: usize,
    bytes: [u8; FRAME_MAX],
}
impl Frame {
    const EMPTY: Self = Self {
        len: 0,
        bytes: [0; FRAME_MAX],
    };
}
struct Port {
    epoch: u32,
    peer: usize,
    active: bool,
    frames: [Frame; DEPTH],
    head: usize,
    len: usize,
    dropped: u64,
}
impl Port {
    const fn new() -> Self {
        Self {
            epoch: 0,
            peer: 0,
            active: false,
            frames: [Frame::EMPTY; DEPTH],
            head: 0,
            len: 0,
            dropped: 0,
        }
    }
    fn clear(&mut self) {
        // Lifecycle/reset only, not the per-frame hot path. No payload bytes or
        // queue indices survive a device reset or a replacement VM epoch.
        self.frames.fill(Frame::EMPTY);
        self.head = 0;
        self.len = 0;
    }
}
pub struct Link {
    ports: [Port; PORTS],
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Snapshot {
    pub up: bool,
    pub queued: usize,
    pub dropped: u64,
}
impl Link {
    pub const fn new() -> Self {
        Self {
            ports: [const { Port::new() }; PORTS],
        }
    }
    pub fn bind(&mut self, slot: usize, epoch: u32, peer: usize) -> Result<Endpoint, Error> {
        if slot >= PORTS || peer >= PORTS || slot == peer || epoch == 0 {
            return Err(Error::Invalid);
        }
        if self.ports[slot].epoch != 0 {
            return Err(Error::Bound);
        }
        if self.ports[peer].epoch != 0 && self.ports[peer].peer != slot {
            return Err(Error::Invalid);
        }
        if self.ports.iter().enumerate().any(|(index, port)| {
            port.epoch != 0
                && ((port.peer == slot && index != peer) || (port.peer == peer && index != slot))
        }) {
            return Err(Error::Invalid);
        }
        self.ports[slot] = Port {
            epoch,
            peer,
            ..Port::new()
        };
        Ok(Endpoint { slot, epoch })
    }
    fn index(&self, endpoint: Endpoint) -> Result<usize, Error> {
        if self.ports[endpoint.slot].epoch != endpoint.epoch {
            return Err(Error::Stale);
        }
        Ok(endpoint.slot)
    }
    pub fn activate(&mut self, endpoint: Endpoint) -> Result<(), Error> {
        let slot = self.index(endpoint)?;
        self.ports[slot].active = true;
        Ok(())
    }
    pub fn reset(&mut self, endpoint: Endpoint) -> Result<(), Error> {
        let slot = self.index(endpoint)?;
        let peer = self.ports[slot].peer;
        self.ports[slot].clear();
        self.ports[peer].clear();
        Ok(())
    }
    pub fn retire(&mut self, endpoint: Endpoint) -> Result<(), Error> {
        self.reset(endpoint)?;
        self.ports[endpoint.slot].active = false;
        self.ports[endpoint.slot].epoch = 0;
        Ok(())
    }
    pub fn snapshot(&self, endpoint: Endpoint) -> Result<Snapshot, Error> {
        let slot = self.index(endpoint)?;
        let port = &self.ports[slot];
        let peer = &self.ports[port.peer];
        Ok(Snapshot {
            up: port.active && peer.active && peer.epoch != 0,
            queued: port.len,
            dropped: port.dropped,
        })
    }
    /// Validate the incarnation once. Exclusive borrowing prevents reset,
    /// retirement or rebinding for the complete synchronous service interval.
    pub fn backend(&mut self, endpoint: Endpoint) -> Result<Backend<'_>, Error> {
        let slot = self.index(endpoint)?;
        Ok(Backend { link: self, slot })
    }
}
pub struct Backend<'a> {
    link: &'a mut Link,
    slot: usize,
}
impl NetBackend for Backend<'_> {
    fn transmit(&mut self, frame: &[u8]) -> Result<(), NetError> {
        // The device protocol already validated Ethernet length before copying.
        // Trusted backend call: the frontend already validated frame bounds.
        debug_assert!((14..=FRAME_MAX).contains(&frame.len()));
        let peer = self.link.ports[self.slot].peer;
        if !self.link.ports[self.slot].active
            || !self.link.ports[peer].active
            || self.link.ports[peer].len == DEPTH
        {
            // Ethernet loss, not a permanently blocked TX descriptor. A down
            // or full peer cannot retain the sender's queue or host worker.
            self.link.ports[self.slot].dropped += 1;
            return Ok(());
        }
        let port = &mut self.link.ports[peer];
        let frame_slot = &mut port.frames[(port.head + port.len) % DEPTH];
        frame_slot.bytes[..frame.len()].copy_from_slice(frame);
        frame_slot.len = frame.len();
        port.len += 1;
        Ok(())
    }
    fn receive(&mut self, output: &mut [u8]) -> Result<Option<usize>, NetError> {
        let port = &mut self.link.ports[self.slot];
        if !port.active || port.len == 0 {
            return Ok(None);
        }
        let frame = &mut port.frames[port.head];
        if output.len() < frame.len {
            return Err(NetError::Fault);
        }
        let len = frame.len;
        output[..len].copy_from_slice(&frame.bytes[..len]);
        frame.len = 0;
        port.head = (port.head + 1) % DEPTH;
        port.len -= 1;
        Ok(Some(len))
    }
}
