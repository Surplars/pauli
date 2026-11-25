//! Sparse grant records with one inline page and optional backend-owned extension pages.
//! Ownership/Lock Ordering: only the management service mutates the chain; no policy locks.
//! Invariants: no authority without a filled record; reuse revoked space before growing.
use crate::{Backend, Error, Rights, VmId};
pub(super) const PER_PAGE: usize = 64;
#[derive(Clone, Copy)]
pub(super) struct Grant {
    pub source: u32,
    pub vm: VmId,
    pub rights: Rights,
    /// LOG and MAIL bind to this incarnation. Lifecycle authority applies
    /// to the configured resource across a cold restart.
    pub target_epoch: u32,
}
#[derive(Clone, Copy)]
pub(super) enum Entry {
    Vm(Grant),
    HostStats(u32),
}
impl Entry {
    /// A host grant has no target VM and cannot match a VM-scoped grant.
    pub(super) fn key(self) -> (u32, Option<VmId>) {
        match self {
            Self::Vm(g) => (g.source, Some(g.vm)),
            Self::HostStats(source) => (source, None),
        }
    }
}
pub struct GrantPage {
    entries: [Option<Entry>; PER_PAGE],
    next: Option<&'static mut GrantPage>,
}
impl GrantPage {
    pub const fn new() -> Self {
        Self {
            entries: [None; PER_PAGE],
            next: None,
        }
    }
    pub(super) fn get(&self, source: u32, vm: Option<VmId>) -> Option<Entry> {
        core::iter::successors(Some(self), |p| p.next.as_deref())
            .flat_map(|p| p.entries.iter().flatten())
            .find(|entry| entry.key() == (source, vm))
            .copied()
    }
    pub(super) fn vm_grant(&self, source: u32, vm: VmId) -> Option<Grant> {
        match self.get(source, Some(vm))? {
            Entry::Vm(g) => Some(g),
            Entry::HostStats(_) => None,
        }
    }
    fn entries_mut(&mut self) -> impl Iterator<Item = &mut Option<Entry>> {
        let mut next = Some(self);
        core::iter::from_fn(move || {
            let page = next.take()?;
            next = page.next.as_deref_mut();
            Some(&mut page.entries)
        })
        .flatten()
    }
    pub(super) fn set(&mut self, grant: Entry, backend: &impl Backend) -> Result<(), Error> {
        if let Some(old) = self
            .entries_mut()
            .flatten()
            .find(|entry| entry.key() == grant.key())
        {
            *old = grant;
            return Ok(());
        }
        *self.vacant(backend)? = Some(grant);
        Ok(())
    }
    pub(super) fn vacant(&mut self, backend: &impl Backend) -> Result<&mut Option<Entry>, Error> {
        let mut page = self;
        loop {
            if let Some(i) = page.entries.iter().position(Option::is_none) {
                return Ok(&mut page.entries[i]);
            }
            page = match &mut page.next {
                Some(next) => next,
                slot @ None => slot.insert(backend.grant_page()?),
            };
        }
    }
    pub(super) fn remove(&mut self, matches: impl Fn(Entry) -> bool) {
        for entry in self.entries_mut() {
            if entry.is_some_and(&matches) {
                *entry = None;
            }
        }
    }
}
