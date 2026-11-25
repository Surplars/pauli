#![no_std]
#![forbid(unsafe_code)]
//! No-heap management policy shared by local and guest transports.
//! Host types are not wire layouts: `abi` defines the scalar guest protocol.
//! The transport derives Caller from its endpoint/vCPU, never request contents.
//! One management owner serializes calls; no lock spans a backend operation.
//! Ownership/Lock Ordering: exclusive service owner, then backend; no internal locks.
//! Invariants: authorize before effects; grants bind both source and target epochs.

#[cfg(test)]
#[macro_use]
extern crate std;
#[cfg(test)]
mod tests;

pub mod abi;
mod grants;
use grants::Grant;
pub use grants::GrantPage;
mod provision;
pub use provision::{ChildName, ChildSpec, DiskSlot, Shape};
pub mod queue;

/// Stable configured resource, independent of its monotonically changing VM
/// epoch. Values are checked against the backend, not used as unchecked indices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmId(pub u32);

/// Trusted transport identity, not an authority the guest may serialize.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Caller {
    Host,
    Guest(u32), // Actual source VM epoch, never a configured slot number.
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rights(u16);
impl Rights {
    pub const INSPECT: Self = Self(1);
    pub const START: Self = Self(2);
    pub const STOP: Self = Self(4);
    pub const CONSOLE: Self = Self(8);
    pub const CREATE: Self = Self(16);
    pub const DESTROY: Self = Self(32);
    /// Modify CPU/RAM at creation. Never implied by ordinary template cloning.
    pub const SHAPE: Self = Self(64);
    /// Read the console history of one target epoch, including after it stops.
    pub const LOG: Self = Self(1 << 7);
    /// Exchange copied messages with a specific peer incarnation.
    pub const MAIL: Self = Self(1 << 12);
    /// Legacy operating rights. Provisioning is a separate explicit grant.
    pub const ALL: Self = Self(15);
    pub const fn disk(slot: DiskSlot) -> Self {
        Self(1 << (8 + slot.index()))
    }
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    /// Host-console input only; guest wire requests never carry grant rights.
    pub fn parse(text: &str) -> Option<Self> {
        if text == "all" {
            return Some(Self::ALL);
        }
        let mut result = None;
        for word in text.split(',') {
            let right = match word {
                "inspect" => Self::INSPECT,
                "start" => Self::START,
                "stop" => Self::STOP,
                "console" => Self::CONSOLE,
                "create" => Self::CREATE,
                "destroy" => Self::DESTROY,
                "shape" => Self::SHAPE,
                "log" => Self::LOG,
                "mail" => Self::MAIL,
                word if word.len() == 5 && word.starts_with("disk") => {
                    Self::disk(DiskSlot::new(word.as_bytes()[4].checked_sub(b'0')?)?)
                }
                _ => return None,
            };
            result = Some(result.map_or(right, |old: Self| old.union(right)));
        }
        result
    }
    const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Created,
    Ready,
    Running,
    Waiting,
    Stopping,
    Stopped,
    Faulted,
}

/// Intentionally no HPA, context pointer, register bank or memory access token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    pub vm: VmId,
    pub epoch: u32,
    pub state: State,
    pub active: bool,
    pub vcpus: u8,
    pub memory_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Inspect,
    MemoryBytes,
    Start,
    Stop,
    Console,
    Input(u8),
    ConsoleRead(u64),
    MailSend(MailMessage),
    MailRecv,
    Create(ChildSpec),
    Destroy,
}
impl Operation {
    const fn right(self) -> Rights {
        match self {
            Self::Inspect | Self::MemoryBytes => Rights::INSPECT,
            Self::Start => Rights::START,
            Self::Stop => Rights::STOP,
            Self::Console | Self::Input(_) => Rights::CONSOLE,
            Self::ConsoleRead(_) => Rights::LOG,
            Self::MailSend(_) | Self::MailRecv => Rights::MAIL,
            Self::Create(spec) => {
                let mut required = Rights::CREATE;
                if spec.shape.vcpus.is_some() || spec.shape.memory_mib.is_some() {
                    required = required.union(Rights::SHAPE);
                }
                if let Some(slot) = spec.disk {
                    required = required.union(Rights::disk(slot));
                }
                required
            }
            Self::Destroy => Rights::DESTROY,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub vm: VmId,
    /// Inspect discovers the epoch. Mail may use zero to select the granted
    /// peer incarnation. All other operations must echo the current epoch.
    pub epoch: u32,
    pub operation: Operation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleChunk {
    Bytes { data: [u8; 6], len: u8 },
    Overrun { resume: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MailMessage {
    pub tag: u8,
    pub len: u8,
    pub data: [u8; 6],
}
impl MailMessage {
    fn canonical(self) -> bool {
        self.data
            .get(usize::from(self.len)..)
            .is_some_and(|padding| padding.iter().all(|&b| b == 0))
    }
}

/// A zero-length message still occupies one entry and is returned as Some.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MailChunk {
    pub message: Option<MailMessage>,
    pub more: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reply {
    pub info: Info,
    /// True only when start/stop actually changed activation state.
    pub changed: bool,
    pub log: Option<ConsoleChunk>,
    pub mail: Option<MailChunk>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Denied,
    NotFound,
    StaleEpoch,
    NotActive,
    NoSpace,
    Busy,
    Unsupported,
    Invalid,
    Backend(&'static str),
}
impl Error {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Denied => "management permission denied",
            Self::NotFound => "unknown guest",
            Self::StaleEpoch => "stale VM epoch; refresh status",
            Self::NotActive => "start the VM before attaching",
            Self::NoSpace => "resource quota, grant table or host capacity exhausted",
            Self::Busy => "resource busy",
            Self::Unsupported => "unsupported resource or operation",
            Self::Invalid => "invalid resource or name",
            Self::Backend(message) => message,
        }
    }
}

/// Trusted lifecycle adapter on the sole management owner, outside vCPU
/// execution and registry/device locks. Lifecycle/input callbacks run only
/// after authorization; read-only info/snapshot also serve identity/discovery.
/// Boot resources stay present. Dynamic resources can be destroyed only after
/// the backend proves CPU, translation, I/O and reader retirement.
pub trait Backend {
    fn snapshot(&self) -> impl Iterator<Item = Info> + '_;
    fn info(&self, vm: VmId) -> Result<Info, Error>;
    fn start(&mut self, vm: VmId) -> Result<bool, Error>;
    fn stop(&mut self, vm: VmId) -> Result<(), Error>;
    fn input(&mut self, vm: VmId, byte: u8) -> Result<(), Error>;
    fn console_read(&self, _vm: VmId, _epoch: u32, _cursor: u64) -> Result<ConsoleChunk, Error> {
        Err(Error::Unsupported)
    }
    fn mail_send(
        &self,
        _sender: VmId,
        _sender_epoch: u32,
        _target: VmId,
        _target_epoch: u32,
        _message: MailMessage,
    ) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
    fn mail_recv(
        &self,
        _receiver: VmId,
        _receiver_epoch: u32,
        _peer: VmId,
        _peer_epoch: u32,
    ) -> Result<MailChunk, Error> {
        Err(Error::Unsupported)
    }
    /// Optional host-only storage for the next batch of grants. Return an empty
    /// page exclusively to this service, or fail before a child is created.
    fn grant_page(&self) -> Result<&'static mut GrantPage, Error> {
        Err(Error::NoSpace)
    }
    // Sponsor is resolved from the authenticated epoch, never a guest-supplied ID.
    fn create(
        &mut self,
        _sponsor: Option<VmId>,
        _template: VmId,
        _spec: ChildSpec,
    ) -> Result<Info, Error> {
        Err(Error::Unsupported)
    }
    fn destroy(&mut self, _vm: VmId, _epoch: u32) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
}

pub struct Service {
    grants: GrantPage,
}
impl Service {
    pub const fn new() -> Self {
        Self {
            grants: GrantPage::new(),
        }
    }

    fn sender(caller: Caller, backend: &impl Backend) -> Result<Option<VmId>, Error> {
        match caller {
            Caller::Host => Ok(None),
            Caller::Guest(source) => backend
                .snapshot()
                .find(|info| info.epoch == source && info.active)
                .map(|info| Some(info.vm))
                .ok_or(Error::Denied),
        }
    }

    fn authorize(&self, caller: Caller, vm: VmId, right: Rights) -> Result<Option<Grant>, Error> {
        match caller {
            Caller::Host => Ok(None),
            Caller::Guest(source) => self
                .grants
                .get(source, vm)
                .filter(|grant| grant.rights.contains(right))
                .map(Some)
                .ok_or(Error::Denied),
        }
    }

    /// Replace, not accumulate, rights for this exact (source epoch, resource).
    /// Only the host provisions grants. A guest cannot delegate its authority.
    pub fn grant(
        &mut self,
        caller: Caller,
        source: u32,
        vm: VmId,
        rights: Rights,
        backend: &impl Backend,
    ) -> Result<(), Error> {
        if caller != Caller::Host {
            return Err(Error::Denied);
        }
        if !backend.snapshot().any(|info| info.epoch == source) {
            return Err(Error::NotFound);
        }
        let target_epoch = backend.info(vm)?.epoch;
        self.grants.set(
            Grant {
                source,
                vm,
                rights,
                target_epoch,
            },
            backend,
        )?;
        Ok(())
    }

    pub fn revoke(&mut self, caller: Caller, source: u32, vm: VmId) -> Result<(), Error> {
        if caller != Caller::Host {
            return Err(Error::Denied);
        }
        self.grants.remove(|g| g.source == source && g.vm == vm);
        Ok(())
    }

    /// Owner notification for guest shutdown/fault, or epoch retirement. No
    /// authority silently transfers to the replacement guest in the same slot.
    pub fn retire(&mut self, source: u32) {
        self.grants.remove(|g| g.source == source);
    }

    /// Hidden VMs are omitted, not exposed as denied records. Snapshot contains
    /// sanitized metadata only; authorization does not grant memory access.
    pub fn list<'a>(
        &'a self,
        caller: Caller,
        backend: &'a impl Backend,
    ) -> impl Iterator<Item = Info> + 'a {
        let current = Self::sender(caller, backend).is_ok();
        backend
            .snapshot()
            .filter(move |info| current && self.authorize(caller, info.vm, Rights::INSPECT).is_ok())
    }

    pub fn execute(
        &mut self,
        caller: Caller,
        request: Request,
        backend: &mut impl Backend,
    ) -> Result<Reply, Error> {
        // Reject before looking up the target or invoking lifecycle/device work.
        let sender = Self::sender(caller, backend)?;
        let right = request.operation.right();
        let grant = self.authorize(caller, request.vm, right)?;
        if right == Rights::MAIL && caller == Caller::Host {
            return Err(Error::Unsupported);
        }
        let before = backend.info(request.vm)?;
        let use_granted_epoch = right == Rights::MAIL && request.epoch == 0;
        if (right == Rights::MAIL || right == Rights::LOG)
            && let Some(expired) = grant.filter(|g| g.target_epoch != before.epoch)
        {
            // The former reader may learn that its own handle expired. Any
            // other guess gets the same denial, including the replacement epoch.
            return Err(
                if use_granted_epoch || request.epoch == expired.target_epoch {
                    Error::StaleEpoch
                } else {
                    Error::Denied
                },
            );
        }
        if request.operation != Operation::Inspect
            && !use_granted_epoch
            && request.epoch != before.epoch
        {
            return Err(Error::StaleEpoch);
        }
        let mut mail = None;
        let (info, changed) = match request.operation {
            Operation::Create(spec) => {
                return self.create_child(caller, sender, request.vm, spec, backend);
            }
            Operation::Destroy => return self.destroy_child(before, backend),
            Operation::ConsoleRead(cursor) => {
                let log = backend.console_read(request.vm, before.epoch, cursor)?;
                return Ok(Reply {
                    info: before,
                    changed: false,
                    log: Some(log),
                    mail: None,
                });
            }
            Operation::MailSend(_) | Operation::MailRecv => {
                // Both endpoint identities come from trusted runtime metadata.
                let (source, grant) = sender.zip(grant).ok_or(Error::Unsupported)?;
                if let Operation::MailSend(message) = request.operation {
                    if !message.canonical() {
                        return Err(Error::Invalid);
                    }
                    backend.mail_send(source, grant.source, before.vm, before.epoch, message)?;
                } else {
                    mail =
                        Some(backend.mail_recv(source, grant.source, before.vm, before.epoch)?);
                }
                (before, false)
            }
            Operation::Inspect | Operation::MemoryBytes => (before, false),
            Operation::Start => {
                let result = backend.start(request.vm);
                // A failed boot can still have retired its old context/RAM.
                // Revoke old credentials even when activation did not succeed.
                let after = backend.info(request.vm)?;
                if after.epoch != before.epoch {
                    self.retire(before.epoch);
                }
                (after, result?)
            }
            Operation::Stop => {
                backend.stop(request.vm)?;
                self.retire(before.epoch);
                (backend.info(request.vm)?, before.active)
            }
            Operation::Console | Operation::Input(_) => {
                if !before.active {
                    return Err(Error::NotActive);
                }
                if let Operation::Input(byte) = request.operation {
                    backend.input(request.vm, byte)?;
                }
                (before, false)
            }
        };
        Ok(Reply {
            info,
            changed,
            log: None,
            mail,
        })
    }
}
