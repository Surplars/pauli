//! Peer Ethernet and exclusive native uplink bridge; all frames are copied.
//! Ownership/Lock Ordering: VM devices -> shared Link/native driver; no reverse calls.
//! Invariants: endpoint epochs reject stale packets; native reset ACK precedes reuse.
use crate::{HvError, Result, manager::leases::LeaseHandle, memory::GuestRam};
use pauli_config::VmConfig;
use pauli_devices::{
    net_link::{ChannelEndpoint, DEPTH, Endpoint, Link, RxReady, Side, Snapshot},
    virtio_net::{NetBackend, NetError, VirtioNet},
};
use pauli_hal::{
    device::{DeviceError, HostNetDevice},
    sync::SpinLock,
};
const REG_STATUS: usize = 0x70;
const REG_QUEUE_NOTIFY: usize = 0x50;
const CONFIG: usize = 0x100;
enum Backend {
    Peer {
        endpoint: Option<Endpoint>,
        slot: usize,
        peer: usize,
        retired: Snapshot,
    },
    Leased {
        lease: LeaseHandle,
        endpoint: Option<ChannelEndpoint>,
        slot: usize,
        side: Side,
    },
    Native {
        device: &'static dyn HostNetDevice,
        active: bool,
    },
}
pub struct Net {
    pub device: VirtioNet,
    backend: Backend,
}
impl Net {
    pub(crate) fn bind(
        slot: usize,
        epoch: u32,
        config: &VmConfig,
        link: &SpinLock<Link>,
        native: Option<&'static dyn HostNetDevice>,
        lease: Option<LeaseHandle>,
    ) -> Result<Option<Self>> {
        if lease.is_some() != config.net_lease.is_some()
            || (lease.is_some() && (config.net_peer.is_some() || config.net_uplink))
        {
            return Err(HvError::InvalidConfig);
        }
        let (mac, backend) = if config.net_uplink {
            let device = native.ok_or(HvError::InvalidDevice)?;
            (
                device.mac_address(),
                Backend::Native {
                    device,
                    active: false,
                },
            )
        } else if let Some(peer) = config.net_peer {
            let endpoint = link
                .lock()
                .bind(slot, epoch, usize::from(peer))
                .map_err(|_| HvError::InvalidConfig)?;
            // Only an admitted peer port has a one-byte MAC suffix; a diskless,
            // networkless domain's runtime ID need not fit in that namespace.
            (
                [2, 0, 0, 0, 0, slot as u8 + 1],
                Backend::Peer {
                    endpoint: Some(endpoint),
                    slot,
                    peer: peer as usize,
                    retired: Snapshot::default(),
                },
            )
        } else if let (Some(binding), Some(lease)) = (config.net_lease, lease) {
            if !binding.is_valid() {
                return Err(HvError::InvalidConfig);
            }
            let side = Side::from_index(binding.side).ok_or(HvError::InvalidConfig)?;
            let ep = lease
                .with_channel(|channel| channel.bind(side, slot, epoch))
                .map_err(|err| {
                    if err == pauli_devices::net_link::Error::Bound {
                        HvError::Busy
                    } else {
                        HvError::InvalidConfig
                    }
                })?;
            (
                side.mac(binding.channel),
                Backend::Leased {
                    lease,
                    endpoint: Some(ep),
                    slot,
                    side,
                },
            )
        } else {
            return Ok(None);
        };
        Ok(Some(Self {
            device: VirtioNet::new(mac),
            backend,
        }))
    }
    pub(crate) fn lease_handle(&self) -> Option<LeaseHandle> {
        match &self.backend {
            Backend::Leased { lease, .. } => Some(lease.clone()),
            _ => None,
        }
    }
    pub(crate) fn leased_peer_slot(&self) -> Option<usize> {
        match &self.backend {
            Backend::Leased { lease, side, .. } => {
                lease.with_channel(|channel| channel.peer_slot(*side))
            }
            _ => None,
        }
    }
    pub fn activate(&mut self, epoch: u32, link: &SpinLock<Link>) -> Result<()> {
        let up = match &mut self.backend {
            Backend::Peer {
                endpoint,
                slot,
                peer,
                ..
            } => {
                let mut link = link.lock();
                let ep = match endpoint {
                    Some(ep) => *ep,
                    None => {
                        let ep = link
                            .bind(*slot, epoch, *peer)
                            .map_err(|_| HvError::InvalidState)?;
                        *endpoint = Some(ep);
                        ep
                    }
                };
                link.activate(ep).map_err(|_| HvError::InvalidState)?;
                link.snapshot(ep).map_err(|_| HvError::InvalidState)?.up
            }
            Backend::Leased {
                lease,
                endpoint,
                slot,
                side,
            } => lease.with_channel(|ch| {
                let ep = match *endpoint {
                    Some(ep) => ep,
                    None => {
                        let ep = ch
                            .bind(*side, *slot, epoch)
                            .map_err(|_| HvError::InvalidState)?;
                        *endpoint = Some(ep);
                        ep
                    }
                };
                ch.activate(ep)
                    .map(|()| ch.side_snapshot(*side).up)
                    .map_err(|_| HvError::InvalidState)
            })?,
            Backend::Native { device, active } => {
                device.restart()?;
                *active = true;
                device.link_up()
            }
        };
        self.device.set_link(up);
        Ok(())
    }
    pub fn stop(&mut self, link: &SpinLock<Link>) -> Result<()> {
        match &mut self.backend {
            Backend::Peer {
                endpoint, retired, ..
            } => {
                if let Some(ep) = *endpoint {
                    let mut link = link.lock();
                    let previous = link.snapshot(ep).map_err(|_| HvError::InvalidState)?;
                    link.retire(ep).map_err(|_| HvError::InvalidState)?;
                    *retired = Snapshot {
                        up: false,
                        queued: 0,
                        ..previous
                    };
                    *endpoint = None;
                }
            }
            Backend::Leased {
                lease, endpoint, ..
            } => {
                if let Some(ep) = *endpoint {
                    lease
                        .with_channel(|ch| ch.retire(ep))
                        .map_err(|_| HvError::InvalidState)?;
                    *endpoint = None;
                }
            }
            Backend::Native { device, active } => {
                if *active {
                    device.stop()?;
                    *active = false;
                }
            }
        }
        self.device.set_link(false);
        self.device
            .write_word(REG_STATUS, 0)
            .ok_or(HvError::InvalidDevice)
    }
    pub fn service(
        &mut self,
        memory: &mut GuestRam,
        link: &SpinLock<Link>,
    ) -> Result<Option<RxReady>> {
        let mut service_peer = |mut backend: pauli_devices::net_link::Backend<'_>| {
            self.device.set_link(backend.link_up());
            self.device.service(memory, &mut backend, DEPTH);
            backend.ready()
        };
        match &mut self.backend {
            Backend::Peer {
                endpoint: Some(ep), ..
            } => {
                return link
                    .lock()
                    .backend(*ep)
                    .map(service_peer)
                    .map_err(|_| HvError::InvalidState);
            }
            Backend::Leased {
                lease,
                endpoint: Some(ep),
                ..
            } => {
                return lease
                    .with_channel(|channel| channel.backend(*ep).map(&mut service_peer))
                    .map_err(|_| HvError::InvalidState);
            }
            Backend::Peer { .. } | Backend::Leased { .. } => {}
            Backend::Native { device, active } => {
                self.device.set_link(*active && device.link_up());
                if *active {
                    self.device.service(memory, &mut Native(*device), DEPTH);
                }
            }
        }
        Ok(None)
    }
    pub fn peer_snapshot(&self, link: &SpinLock<Link>) -> Option<Snapshot> {
        match &self.backend {
            Backend::Peer {
                endpoint: Some(ep), ..
            } => link.lock().snapshot(*ep).ok(),
            Backend::Peer { retired, .. } => Some(*retired),
            Backend::Leased { lease, side, .. } => {
                Some(lease.with_channel(|ch| ch.side_snapshot(*side)))
            }
            Backend::Native { .. } => None,
        }
    }
    pub fn refresh_carrier(&mut self, link: &SpinLock<Link>) {
        if let Some(snapshot) = self.peer_snapshot(link) {
            self.device.set_link(snapshot.up);
        }
    }
    pub fn receive_peer(
        &mut self,
        mem: &mut GuestRam,
        link: &SpinLock<Link>,
        hint: RxReady,
    ) -> bool {
        let device = &mut self.device;
        let mut receive = |backend: Option<pauli_devices::net_link::Backend<'_>>| {
            let Some(mut rx) = backend else { return false };
            device.service_receive(mem, &mut rx, DEPTH);
            true
        };
        match &self.backend {
            Backend::Peer {
                endpoint: Some(ep), ..
            } => receive(link.lock().receiver(*ep, hint)),
            Backend::Leased {
                lease,
                endpoint: Some(ep),
                ..
            } => lease.with_channel(|ch| receive(ch.receiver(*ep, hint))),
            _ => false,
        }
    }
    pub fn notified(&self, offset: usize, write: Option<usize>) -> bool {
        offset == REG_QUEUE_NOTIFY
            && matches!(write, Some(0 | 1))
            && matches!(self.backend, Backend::Peer { .. } | Backend::Leased { .. })
    }
    pub fn access(
        &mut self,
        offset: usize,
        width: usize,
        write: Option<usize>,
        link: &SpinLock<Link>,
    ) -> Option<usize> {
        if let Some(value) = write {
            if width != 4 {
                return None;
            }
            if offset == REG_STATUS && value == 0 {
                match &mut self.backend {
                    Backend::Peer { endpoint, .. } => {
                        if let Some(ep) = endpoint {
                            link.lock().reset(*ep).ok()?;
                        }
                    }
                    Backend::Leased {
                        lease, endpoint, ..
                    } => {
                        if let Some(ep) = endpoint {
                            lease.with_channel(|ch| ch.reset(*ep)).ok()?;
                        }
                    }
                    Backend::Native { device, active } => {
                        if *active {
                            device.restart().ok()?;
                        }
                    }
                }
            }
            self.device.write_word(offset, value as u32).map(|()| 0)
        } else if offset >= CONFIG {
            let mut value = 0;
            for byte in 0..width {
                value |= usize::from(self.device.read_config_byte(offset - CONFIG + byte)?)
                    << (byte * 8);
            }
            Some(value)
        } else if width == 4 {
            self.device.read_word(offset).map(|v| v as usize)
        } else {
            None
        }
    }
}
impl Drop for Net {
    fn drop(&mut self) {
        // Unstarted children still have a bound inactive side. Started children
        // must have retired through the ordinary joined-device barrier first.
        if let Backend::Leased {
            lease,
            endpoint: Some(ep),
            ..
        } = &self.backend
        {
            let retired = lease.with_channel(|channel| channel.retire(*ep));
            debug_assert!(retired.is_ok());
        }
    }
}
struct Native(&'static dyn HostNetDevice);
fn error(error: DeviceError) -> NetError {
    if error == DeviceError::Busy {
        NetError::Busy
    } else {
        NetError::Fault
    }
}
impl NetBackend for Native {
    fn transmit(&mut self, frame: &[u8]) -> core::result::Result<(), NetError> {
        self.0.transmit(frame).map_err(error)
    }
    fn receive(&mut self, out: &mut [u8]) -> core::result::Result<Option<usize>, NetError> {
        self.0.receive(out).map_err(error)
    }
}
