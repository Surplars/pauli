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
    /// LOG is specific to the bytes of this incarnation. Lifecycle authority
    /// still applies to the configured resource across a cold restart.
    pub target_epoch: u32,
}
pub struct GrantPage {
    entries: [Option<Grant>; PER_PAGE],
    next: Option<&'static mut GrantPage>,
}
impl GrantPage {
    pub const fn new() -> Self {
        Self {
            entries: [None; PER_PAGE],
            next: None,
        }
    }
    pub(super) fn get(&self, source: u32, vm: VmId) -> Option<Grant> {
        core::iter::successors(Some(self), |p| p.next.as_deref())
            .flat_map(|p| p.entries.iter().flatten())
            .find(|g| g.source == source && g.vm == vm)
            .copied()
    }
    fn entries_mut(&mut self) -> impl Iterator<Item = &mut Option<Grant>> {
        let mut next = Some(self);
        core::iter::from_fn(move || {
            let page = next.take()?;
            next = page.next.as_deref_mut();
            Some(&mut page.entries)
        })
        .flatten()
    }
    pub(super) fn set(&mut self, grant: Grant, backend: &impl Backend) -> Result<(), Error> {
        if let Some(old) = self
            .entries_mut()
            .flatten()
            .find(|g| g.source == grant.source && g.vm == grant.vm)
        {
            *old = grant;
            return Ok(());
        }
        *self.vacant(backend)? = Some(grant);
        Ok(())
    }
    pub(super) fn vacant(&mut self, backend: &impl Backend) -> Result<&mut Option<Grant>, Error> {
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
    pub(super) fn remove(&mut self, matches: impl Fn(Grant) -> bool) {
        for entry in self.entries_mut() {
            if entry.is_some_and(&matches) {
                *entry = None;
            }
        }
    }
}
