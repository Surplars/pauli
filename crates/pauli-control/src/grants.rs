//! Sparse grant records with one inline page and optional backend-owned extension pages.
//! Ownership/Lock Ordering: only the management service mutates the chain; no policy locks.
//! Invariants: no authority without a filled record; reuse revoked space before growing.
use crate::{Backend, Error, Rights, VmId};
pub(super) const PER_PAGE: usize = 64;
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MailWatch {
    /// No MAIL right, or the target epoch's retirement was already reported.
    None,
    /// This grant authorizes its holder to poll for this peer.
    Peer,
    /// Both endpoint epochs authorize each other. Arrival may publish a hint.
    Mutual,
}
#[derive(Clone, Copy)]
pub(super) struct Grant {
    pub source: u32,
    pub source_vm: VmId,
    pub vm: VmId,
    pub rights: Rights,
    /// LOG and MAIL bind to this incarnation. Lifecycle authority applies
    /// to the configured resource across a cold restart.
    pub target_epoch: u32,
    /// INSPECT follows this resource across a cold restart. LOG remains bound
    /// to target_epoch until the host grants it for a new incarnation.
    pub observed_epoch: u32,
    /// Maintained on the management owner so sends need no subscriber search.
    pub mail_watch: MailWatch,
}
impl Grant {
    pub(super) fn observations(self) -> u64 {
        let state = self.rights.contains(Rights::INSPECT);
        let log = self.rights.contains(Rights::LOG) && self.target_epoch == self.observed_epoch;
        u64::from(state) * crate::abi::EVENT_STATE | u64::from(log) * crate::abi::EVENT_LOG
    }
    pub(super) fn change_observations(self, before: u64, after: u64, backend: &impl Backend) {
        if before != after {
            backend.event_watch(
                (self.source_vm, self.source),
                (self.vm, self.observed_epoch),
                before,
                after,
                false,
            );
        }
    }
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
    pub(super) fn vm_grants_mut(&mut self) -> impl Iterator<Item = &mut Grant> {
        self.entries_mut()
            .flatten()
            .filter_map(|entry| match entry {
                Entry::Vm(g) => Some(g),
                Entry::HostStats(_) => None,
            })
    }
    pub(super) fn set_reverse_watch(&mut self, g: Grant, watch: MailWatch) {
        if g.source == g.target_epoch {
            return; // A self-grant has only one record.
        }
        if let Some(reverse) = self.vm_grants_mut().find(|r| {
            r.source == g.target_epoch && r.vm == g.source_vm && r.target_epoch == g.source
        }) {
            reverse.mail_watch = watch;
        }
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
