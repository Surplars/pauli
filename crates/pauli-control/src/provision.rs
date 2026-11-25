//! Resource names and authority for creating and retiring managed VM instances.
//! Ownership/Lock Ordering: the management owner serializes grants and all effects.
//! Invariants: no child inherits template devices; no grant survives target slot reuse.
use super::*;
use core::num::{NonZeroU8, NonZeroU16};

/// A declared logical volume, never a host path or device address. The host
/// grants each slot separately; the selected volume fixes the access mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskSlot(u8);
impl DiskSlot {
    pub const COUNT: usize = 4;
    pub fn new(n: u8) -> Option<Self> {
        (usize::from(n) < Self::COUNT).then_some(Self(n))
    }
    pub const fn index(self) -> u8 {
        self.0
    }
}

/// Missing fields inherit the trusted template. Physical addresses and affinity
/// are deliberately not request fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Shape {
    pub vcpus: Option<NonZeroU8>,
    pub memory_mib: Option<NonZeroU16>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChildSpec {
    pub name: ChildName,
    pub shape: Shape,
    pub disk: Option<DiskSlot>,
}
impl From<ChildName> for ChildSpec {
    fn from(name: ChildName) -> Self {
        Self {
            name,
            shape: Shape::default(),
            disk: None,
        }
    }
}

/// Two scalar words, canonical zero padding, no guest pointer or host path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChildName([u8; 16]);
impl ChildName {
    pub fn new(text: &str) -> Option<Self> {
        let mut bytes = [0; 16];
        bytes
            .get_mut(..text.len())?
            .copy_from_slice(text.as_bytes());
        let name = Self::checked(bytes)?;
        (name.as_str() == text).then_some(name)
    }
    pub fn from_words(lo: u64, hi: u64) -> Option<Self> {
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&lo.to_le_bytes());
        bytes[8..].copy_from_slice(&hi.to_le_bytes());
        Self::checked(bytes)
    }
    fn checked(bytes: [u8; 16]) -> Option<Self> {
        let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        (bytes[0].is_ascii_alphabetic()
            && bytes[..len]
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            && bytes[len..].iter().all(|&b| b == 0))
        .then_some(Self(bytes))
    }
    pub fn as_str(&self) -> &str {
        let len = self.0.iter().position(|&b| b == 0).unwrap_or(self.0.len());
        // Private checked bytes are canonical ASCII.
        core::str::from_utf8(&self.0[..len]).unwrap()
    }
}

impl Service {
    pub(super) fn create_child(
        &mut self,
        caller: Caller,
        sponsor: Option<VmId>,
        template: VmId,
        spec: ChildSpec,
        backend: &mut impl Backend,
    ) -> Result<Reply, Error> {
        // Reserve room for the creator's authority before any irreversible effect.
        let child_grant = match caller {
            Caller::Host => None,
            Caller::Guest(source) => Some((
                source,
                sponsor.ok_or(Error::Denied)?,
                self.grants.vacant(backend)?,
            )),
        };
        let info = backend.create(sponsor, template, spec)?;
        if let Some((source, source_vm, entry)) = child_grant {
            *entry = Some(grants::Entry::Vm(Grant {
                source,
                source_vm,
                vm: info.vm,
                rights: Rights::ALL
                    .union(Rights::DESTROY)
                    .union(Rights::LOG)
                    .union(Rights::MAIL),
                target_epoch: info.epoch,
                mail_watch: MailWatch::Peer,
            }));
        }
        Ok(Reply {
            info,
            changed: true,
            log: None,
            mail: None,
        })
    }

    pub(super) fn destroy_child(
        &mut self,
        before: Info,
        backend: &mut impl Backend,
    ) -> Result<Reply, Error> {
        backend.destroy(before.vm, before.epoch)?;
        // Both directions matter: a newly allocated slot is not the old target,
        // even for other live managers that can inspect its replacement epoch.
        self.retire(before.epoch, backend);
        self.grants.remove(|entry| entry.key().1 == Some(before.vm));
        Ok(Reply {
            info: before,
            changed: true,
            log: None,
            mail: None,
        })
    }
}
