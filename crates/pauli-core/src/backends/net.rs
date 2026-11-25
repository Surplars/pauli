//! Peer Ethernet and exclusive native uplink bridge; all frames are copied.
//! Ownership/Lock Ordering: VM devices -> shared Link/native driver; no reverse calls.
//! Invariants: endpoint epochs reject stale packets; native reset ACK precedes reuse.
use crate::{HvError, Result, memory::GuestRam};
use pauli_config::VmConfig;
use pauli_devices::{
    net_link::{DEPTH, Endpoint, Link, RxReady, Snapshot},
    virtio_net::{NetBackend, NetError, VirtioNet},
};
use pauli_hal::{
    device::{DeviceError, HostNetDevice},
    sync::SpinLock,
};
const REG_STATUS: usize = 0x70;
const REG_QUEUE_NOTIFY: usize = 0x50;
const CONFIG: usize = 0x100;
pub enum Backend {
    Peer {
        endpoint: Option<Endpoint>,
        slot: usize,
        peer: usize,
        retired: Snapshot,
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
    pub fn bind(
        slot: usize,
        epoch: u32,
        config: &VmConfig,
        link: &SpinLock<Link>,
        native: Option<&'static dyn HostNetDevice>,
    ) -> Result<Option<Self>> {
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
        } else {
            return Ok(None);
        };
        Ok(Some(Self {
            device: VirtioNet::new(mac),
            backend,
        }))
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
        match &mut self.backend {
            Backend::Peer { endpoint, .. } => {
                let Some(ep) = endpoint else {
                    return Ok(None);
                };
                let mut link = link.lock();
                self.device
                    .set_link(link.snapshot(*ep).map_err(|_| HvError::InvalidState)?.up);
                let mut backend = link.backend(*ep).map_err(|_| HvError::InvalidState)?;
                self.device.service(memory, &mut backend, DEPTH);
                return Ok(backend.ready());
            }
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
        let Backend::Peer {
            endpoint: Some(ep), ..
        } = &self.backend
        else {
            return false;
        };
        let mut link = link.lock();
        let Some(mut rx) = link.receiver(*ep, hint) else {
            return false;
        };
        self.device.service_receive(mem, &mut rx, DEPTH);
        true
    }
    pub fn notified(&self, offset: usize, write: Option<usize>) -> bool {
        offset == REG_QUEUE_NOTIFY
            && matches!(write, Some(0 | 1))
            && matches!(self.backend, Backend::Peer { .. })
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
