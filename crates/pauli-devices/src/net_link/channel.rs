//! A copied, two-ended link whose guest slots are independent of the boot port table.
use super::{Backend, Endpoint, Error, Port, RxReady, Snapshot};
#[cfg(test)]
mod tests;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    A,
    B,
}
impl Side {
    pub fn from_index(side: u8) -> Option<Self> {
        match side {
            0 => Some(Self::A),
            1 => Some(Self::B),
            _ => None,
        }
    }

    fn other(self) -> usize {
        1 - self as usize
    }

    pub const fn mac(self, channel: u8) -> [u8; 6] {
        [0x06, 0, 0, 0, channel, self as u8]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelEndpoint {
    incarnation: u32,
    binding: u64,
    owner: Endpoint,
    side: Side,
}

/// The serialized allocation belongs to the host. A fully zeroed allocation is a valid,
/// inert Channel. Keep this guarantee for the pool initializer: all fields and padding
/// may be cleared in place before registering a fresh incarnation.
pub struct Channel {
    ports: [Port; 2],
    owners: [usize; 2],
    bindings: [u64; 2],
    incarnation: u32,
}

impl Channel {
    pub const fn new(incarnation: u32) -> Self {
        Self {
            ports: [const { Port::new() }; 2],
            owners: [0; 2],
            bindings: [0; 2],
            incarnation,
        }
    }
    /// Supply a unique nonzero incarnation for each newly zeroed pool allocation.
    /// Also covers immediate reuse of the same channel ID and physical address.
    pub fn initialize(&mut self, incarnation: u32) {
        assert!(self.incarnation == 0 && incarnation != 0);
        self.incarnation = incarnation;
    }

    pub fn bind(
        &mut self,
        side: Side,
        owner_slot: usize,
        epoch: u32,
    ) -> Result<ChannelEndpoint, Error> {
        let i = side as usize;
        let j = side.other();
        if epoch == 0
            || self.incarnation == 0
            || (self.ports[j].epoch != 0 && self.owners[j] == owner_slot)
        {
            return Err(Error::Invalid);
        }
        let port = &mut self.ports[i];
        if port.epoch != 0 {
            return Err(Error::Bound);
        }
        // Binding identity is stable across negotiated VirtIO resets. The
        // separate port generation invalidates in-flight receive notifications.
        self.bindings[i] = self.bindings[i].checked_add(1).ok_or(Error::Invalid)?;
        self.owners[i] = owner_slot;
        port.epoch = epoch;
        port.tx = 0;
        port.rx = 0;
        port.full = 0;
        port.down = 0;
        Ok(ChannelEndpoint {
            incarnation: self.incarnation,
            binding: self.bindings[i],
            owner: Endpoint {
                slot: owner_slot,
                epoch,
            },
            side,
        })
    }

    fn index(&self, ep: ChannelEndpoint) -> Result<usize, Error> {
        let i = ep.side as usize;
        if self.incarnation != ep.incarnation
            || self.bindings[i] != ep.binding
            || self.ports[i].epoch != ep.owner.epoch
            || self.owners[i] != ep.owner.slot
        {
            return Err(Error::Stale);
        }
        Ok(i)
    }

    pub fn activate(&mut self, ep: ChannelEndpoint) -> Result<(), Error> {
        let i = self.index(ep)?;
        self.ports[i].active = true;
        Ok(())
    }

    pub fn reset(&mut self, ep: ChannelEndpoint) -> Result<(), Error> {
        self.index(ep)?;
        // Fail closed before changing either queue if its stamp cannot advance.
        if self.ports.iter().any(|p| p.generation == u64::MAX) {
            return Err(Error::Invalid);
        }
        for port in &mut self.ports {
            port.clear();
        }
        Ok(())
    }

    pub fn retire(&mut self, ep: ChannelEndpoint) -> Result<Snapshot, Error> {
        self.reset(ep)?;
        let port = &mut self.ports[ep.side as usize];
        port.active = false;
        port.epoch = 0;
        Ok(self.side_snapshot(ep.side))
    }

    pub fn snapshot(&self, ep: ChannelEndpoint) -> Result<Snapshot, Error> {
        self.index(ep)?;
        Ok(self.side_snapshot(ep.side))
    }

    /// Last counters remain readable by the side's lease holder through join.
    pub fn side_snapshot(&self, side: Side) -> Snapshot {
        self.ports[side as usize].snapshot(&self.ports[side.other()])
    }

    pub fn peer_slot(&self, side: Side) -> Option<usize> {
        let j = side.other();
        (self.ports[j].epoch != 0).then_some(self.owners[j])
    }

    pub fn backend(&mut self, ep: ChannelEndpoint) -> Result<Backend<'_>, Error> {
        let i = self.index(ep)?;
        let j = ep.side.other();
        let [source, target] = self
            .ports
            .get_disjoint_mut([i, j])
            .map_err(|_| Error::Invalid)?;
        Ok(Backend::new(
            source,
            target,
            self.owners[i],
            self.owners[j],
            self.incarnation,
        ))
    }

    pub fn receiver(&mut self, ep: ChannelEndpoint, hint: RxReady) -> Option<Backend<'_>> {
        let i = self.index(ep).ok()?;
        let j = ep.side.other();
        let port = &self.ports[i];
        let sender = &self.ports[j];
        if self.incarnation != hint.incarnation
            || ep.owner != hint.receiver
            || self.owners[j] != hint.sender.slot
            || sender.epoch != hint.sender.epoch
            || !sender.active
            || !port.active
            || port.len == 0
            || port.generation != hint.generation
        {
            return None;
        }
        self.backend(ep).ok()
    }

    /// Clear the whole allocation, including consumed frames and control data,
    /// before the allocator makes its backing available to another owner.
    pub fn scrub(&mut self) {
        for port in &mut self.ports {
            port.clear();
            port.generation = 0;
            port.peer = 0;
            port.active = false;
            port.epoch = 0;
            port.tx = 0;
            port.rx = 0;
            port.full = 0;
            port.down = 0;
        }
        self.owners = [0; 2];
        self.bindings = [0; 2];
        self.incarnation = 0;
    }
}
