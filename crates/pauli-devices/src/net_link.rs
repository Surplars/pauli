//! Isolated point-to-point Ethernet transport. Host-owned frames only: no guest
//! addresses, DMA, interrupts or callbacks. The board serializes each Link.
//! Ownership/Lock Ordering: VM devices -> externally serialized link; no locks here.
//! Invariants: endpoint epochs cannot consume or publish a replacement VM's frames.
//! Lifecycle: unbound -> bound -> active -> retired; reset clears both directions.
use crate::virtio_net::{FRAME_MAX, NetBackend, NetError};

mod channel;
pub use channel::{Channel, ChannelEndpoint, Side};

pub const PORTS: usize = 4;
pub const DEPTH: usize = 16;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Endpoint {
    slot: usize,
    epoch: u32,
}
/// A committed RX batch for one incarnation of this fixed pair. Resetting
/// either device invalidates the hint, even without changing a VM epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxReady {
    incarnation: u32,
    sender: Endpoint,
    receiver: Endpoint,
    generation: u64,
}
impl RxReady {
    pub fn slot(self) -> usize {
        self.receiver.slot
    }
    pub fn epoch(self) -> u32 {
        self.receiver.epoch
    }
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
    generation: u64,
    tx: u64,
    rx: u64,
    full: u64,
    down: u64,
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
            generation: 0,
            tx: 0,
            rx: 0,
            full: 0,
            down: 0,
        }
    }
    fn snapshot(&self, peer: &Port) -> Snapshot {
        Snapshot {
            up: self.active && peer.active,
            queued: self.len,
            tx: self.tx,
            rx: self.rx,
            dropped: self.full.saturating_add(self.down),
            full: self.full,
            down: self.down,
        }
    }
    fn clear(&mut self) {
        // Lifecycle/reset only, not the per-frame hot path. No payload bytes or
        // queue indices survive a device reset or a replacement VM epoch.
        self.frames.fill(Frame::EMPTY);
        self.head = 0;
        self.len = 0;
        self.generation = self.generation.wrapping_add(1);
    }
}
pub struct Link {
    ports: [Port; PORTS],
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub up: bool,
    pub queued: usize,
    pub dropped: u64,
    pub tx: u64,
    pub rx: u64,
    pub full: u64,
    pub down: u64,
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
        // Never materialize the whole frame store on the boot stack. A free
        // port starts empty and retire erased both directions before reuse.
        let port = &mut self.ports[slot];
        port.epoch = epoch;
        port.peer = peer;
        port.tx = 0;
        port.rx = 0;
        port.down = 0;
        port.full = 0;
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
        Ok(port.snapshot(peer))
    }
    /// Validate the incarnation once. Exclusive borrowing prevents reset,
    /// retirement or rebinding for the complete synchronous service interval.
    pub fn backend(&mut self, endpoint: Endpoint) -> Result<Backend<'_>, Error> {
        let slot = self.index(endpoint)?;
        let peer = self.ports[slot].peer;
        let [source, target] = self
            .ports
            .get_disjoint_mut([slot, peer])
            .map_err(|_| Error::Invalid)?;
        Ok(Backend::new(source, target, slot, peer, 0))
    }
    /// Revalidate under the link lock before touching the receiving guest's
    /// rings. This receive-only pass cannot generate a reverse TX notification.
    pub fn receiver(&mut self, endpoint: Endpoint, hint: RxReady) -> Option<Backend<'_>> {
        if hint.incarnation != 0 {
            return None;
        }
        let port = &self.ports[endpoint.slot];
        let sender = &self.ports[hint.sender.slot];
        if endpoint != hint.receiver
            || port.epoch != endpoint.epoch
            || !port.active
            || port.generation != hint.generation
            || port.len == 0
            || port.peer != hint.sender.slot
            || sender.epoch != hint.sender.epoch
            || !sender.active
        {
            return None;
        }
        self.backend(endpoint).ok()
    }
}
pub struct Backend<'a> {
    source: &'a mut Port,
    target: &'a mut Port,
    source_slot: usize,
    target_slot: usize,
    incarnation: u32,
    enqueued: bool,
}
impl<'a> Backend<'a> {
    fn new(
        source: &'a mut Port,
        target: &'a mut Port,
        source_slot: usize,
        target_slot: usize,
        incarnation: u32,
    ) -> Self {
        Self {
            source,
            target,
            source_slot,
            target_slot,
            incarnation,
            enqueued: false,
        }
    }
    pub fn link_up(&self) -> bool {
        self.source.active && self.target.active
    }
    pub fn ready(&self) -> Option<RxReady> {
        self.enqueued.then_some(RxReady {
            incarnation: self.incarnation,
            sender: Endpoint {
                slot: self.source_slot,
                epoch: self.source.epoch,
            },
            receiver: Endpoint {
                slot: self.target_slot,
                epoch: self.target.epoch,
            },
            generation: self.target.generation,
        })
    }
}
impl NetBackend for Backend<'_> {
    fn transmit(&mut self, frame: &[u8]) -> Result<(), NetError> {
        // The device protocol already validated Ethernet length before copying.
        // Trusted backend call: the frontend already validated frame bounds.
        debug_assert!((14..=FRAME_MAX).contains(&frame.len()));
        if !self.link_up() {
            // Ethernet loss, not a permanently blocked TX descriptor. A down
            // or full peer cannot retain the sender's queue or host worker.
            self.source.down = self.source.down.saturating_add(1);
            return Ok(());
        }
        if self.target.len == DEPTH {
            self.source.full = self.source.full.saturating_add(1);
            return Ok(());
        }
        let port = &mut self.target;
        let frame_slot = &mut port.frames[(port.head + port.len) % DEPTH];
        frame_slot.bytes[..frame.len()].copy_from_slice(frame);
        frame_slot.len = frame.len();
        port.len += 1;
        self.source.tx = self.source.tx.saturating_add(1);
        self.enqueued = true;
        Ok(())
    }
    fn receive(&mut self, output: &mut [u8]) -> Result<Option<usize>, NetError> {
        let port = &mut self.source;
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
        port.rx = port.rx.saturating_add(1);
        Ok(Some(len))
    }
}
