//! Many scalar CREATE operations through the real permission service.
//! Ownership/Lock Ordering: one management owner; the fixture supplies only resources and metadata.
//! Invariants: reserve authority before effects, never authorize a peer or stale slot after reuse.
use super::*;
use std::boxed::Box;

struct Domains {
    entries: Vec<Option<Info>>,
    epoch: u32,
    pages: Cell<usize>,
    creates: usize,
    reject: bool,
}
fn row(id: u32, epoch: u32, active: bool) -> Info {
    Info {
        vm: VmId(id),
        epoch,
        state: if active { State::Ready } else { State::Stopped },
        active,
        vcpus: 1,
        memory_bytes: 64 << 20,
    }
}
impl Domains {
    fn new(pages: usize) -> Self {
        Self {
            entries: (1..=3).map(|id| Some(row(id, id, true))).collect(),
            epoch: 4,
            pages: Cell::new(pages),
            creates: 0,
            reject: false,
        }
    }
}
impl Backend for Domains {
    fn snapshot(&self) -> impl Iterator<Item = Info> + '_ {
        self.entries.iter().flatten().copied()
    }
    fn info(&self, vm: VmId) -> Result<Info, Error> {
        vm.0.checked_sub(1)
            .and_then(|v| self.entries.get(v as usize))
            .copied()
            .flatten()
            .ok_or(Error::NotFound)
    }
    fn start(&mut self, _: VmId) -> Result<bool, Error> {
        Err(Error::Unsupported)
    }
    fn stop(&mut self, _: VmId) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
    fn input(&mut self, _: VmId, _: u8) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
    fn console_read(&self, _: VmId, _: u32, cursor: u64) -> Result<ConsoleChunk, Error> {
        super::logs::read_sample(cursor)
    }
    fn mail_recv(
        &self,
        receiver: VmId,
        epoch: u32,
        peer: VmId,
        peer_epoch: u32,
    ) -> Result<MailChunk, Error> {
        assert_eq!((receiver, epoch), (VmId(1), 1));
        assert_eq!(self.info(peer)?.epoch, peer_epoch);
        Ok(MailChunk {
            message: None,
            more: false,
        })
    }
    fn grant_page(&self) -> Result<&'static mut GrantPage, Error> {
        self.pages
            .set(self.pages.get().checked_sub(1).ok_or(Error::NoSpace)?);
        Ok(Box::leak(Box::new(GrantPage::new())))
    }
    fn create(
        &mut self,
        sponsor: Option<VmId>,
        template: VmId,
        _: ChildSpec,
    ) -> Result<Info, Error> {
        assert_eq!((sponsor, template), (Some(VmId(1)), VmId(2)));
        if self.reject {
            return Err(Error::NoSpace);
        }
        let index = self
            .entries
            .iter()
            .position(Option::is_none)
            .unwrap_or(self.entries.len());
        let child = row(index as u32 + 1, self.epoch, false);
        if index == self.entries.len() {
            self.entries.push(None);
        }
        self.entries[index] = Some(child);
        self.epoch += 1;
        self.creates += 1;
        Ok(child)
    }
    fn destroy(&mut self, id: VmId, epoch: u32) -> Result<(), Error> {
        let child = self.info(id)?;
        if child.epoch != epoch || child.active {
            return Err(Error::Busy);
        }
        self.entries[id.0 as usize - 1] = None;
        Ok(())
    }
}
fn create(s: &mut Service, b: &mut Domains) -> Result<Info, Error> {
    let name = std::format!("unit{}", b.creates + 1);
    let op = Operation::Create(ChildName::new(&name).unwrap().into());
    let reply = s.execute(Caller::Guest(1), request(2, 2, op), b)?;
    // Exercise the same public scalar reply consumed by a management VM.
    assert_eq!(
        abi::encode_for(op, Ok(reply)).value,
        (u64::from(reply.info.vm.0) << 32) | u64::from(reply.info.epoch)
    );
    Ok(reply.info)
}
#[test]
fn directory_growth_preserves_create_revoke_and_epoch_bound_destruction() {
    let mut b = Domains::new(1);
    let mut s = Service::new();
    let admin = Caller::Guest(1);
    let peer = Caller::Guest(3);
    s.grant(Caller::Host, 1, VmId(2), Rights::CREATE, &b)
        .unwrap();
    let first = create(&mut s, &mut b).unwrap();
    let mut high = first;
    for _ in 1..97 {
        high = create(&mut s, &mut b).unwrap();
    }
    assert_eq!(b.pages.get(), 0);
    assert_eq!(s.list(admin, &b).count(), 97);
    let inspect = request(high.vm.0, 0, Operation::Inspect);
    let read = request(high.vm.0, high.epoch, Operation::ConsoleRead(0));
    assert_eq!(
        s.execute(admin, read, &mut b).unwrap().log,
        Some(super::logs::read_sample(0).unwrap())
    );
    assert_eq!(s.execute(peer, read, &mut b), Err(Error::Denied));
    let recv = request(high.vm.0, 0, Operation::MailRecv);
    assert_eq!(
        s.execute(admin, recv, &mut b).unwrap().mail,
        Some(MailChunk {
            message: None,
            more: false
        })
    );
    assert_eq!(s.execute(peer, recv, &mut b), Err(Error::Denied));
    assert_eq!(s.execute(peer, inspect, &mut b), Err(Error::Denied));
    s.grant(Caller::Host, 3, high.vm, Rights::INSPECT, &b)
        .unwrap();
    assert_eq!(s.execute(peer, inspect, &mut b).unwrap().info, high);
    s.revoke(Caller::Host, 1, high.vm).unwrap();
    assert_eq!(s.execute(admin, inspect, &mut b), Err(Error::Denied));
    s.grant(Caller::Host, 1, high.vm, Rights::DESTROY, &b)
        .unwrap();
    let destroy = request(high.vm.0, high.epoch, Operation::Destroy);
    s.execute(admin, destroy, &mut b).unwrap();
    let fresh = create(&mut s, &mut b).unwrap();
    assert_eq!(fresh.vm, high.vm);
    assert_ne!(fresh.epoch, high.epoch);
    assert_eq!(s.execute(admin, destroy, &mut b), Err(Error::StaleEpoch));
    assert_eq!(s.execute(peer, inspect, &mut b), Err(Error::Denied));
    s.retire(1);
    assert_eq!(s.list(admin, &b).count(), 0);
    assert_eq!(b.info(first.vm), Ok(first));
}

#[test]
fn full_or_failed_grant_expansion_never_leaves_an_unmanageable_child() {
    let mut b = Domains::new(0);
    let mut s = Service::new();
    s.grant(Caller::Host, 1, VmId(2), Rights::CREATE, &b)
        .unwrap();
    for _ in 1..grants::PER_PAGE {
        create(&mut s, &mut b).unwrap();
    }
    let before = b.creates;
    assert_eq!(create(&mut s, &mut b), Err(Error::NoSpace));
    assert_eq!(b.creates, before); // Page denied before touching the template backend.
    b.pages.set(1);
    b.reject = true;
    assert_eq!(create(&mut s, &mut b), Err(Error::NoSpace));
    b.reject = false;
    for _ in 0..grants::PER_PAGE {
        create(&mut s, &mut b).unwrap();
    }
    assert_eq!(create(&mut s, &mut b), Err(Error::NoSpace));
    assert_eq!(b.creates, 2 * grants::PER_PAGE - 1);
    assert_eq!(s.list(Caller::Guest(1), &b).count(), b.creates);
}

#[test]
fn creator_stats_authority_never_transfers_to_children_or_slot_replacements() {
    let mut b = Domains::new(0);
    let mut s = Service::new();
    s.grant(Caller::Host, 1, VmId(2), Rights::CREATE, &b)
        .unwrap();
    s.grant_stats(Caller::Host, 1, &b).unwrap();
    let call = Call::StatsRead(StatsWord::new(0, 0, 0).unwrap());
    let child = create(&mut s, &mut b).unwrap();
    let slot = child.vm.0 as usize - 1;
    b.entries[slot] = Some(row(child.vm.0, child.epoch, true));
    let caller = Caller::Guest(child.epoch);
    assert_eq!(
        s.execute_call(caller, call, &mut b),
        abi::Return::error(abi::DENIED)
    );
    // This backend has no collector, so NOT_SUPPORTED distinguishes an actual grant.
    s.grant_stats(Caller::Host, child.epoch, &b).unwrap();
    assert_eq!(
        s.execute_call(caller, call, &mut b),
        abi::Return::error(abi::NOT_SUPPORTED)
    );
    b.entries[slot] = Some(child);
    s.execute(
        Caller::Guest(1),
        request(child.vm.0, child.epoch, Operation::Destroy),
        &mut b,
    )
    .unwrap();
    assert!(s.grants.get(child.epoch, None).is_none());
    assert_eq!(
        s.execute_call(Caller::Guest(1), call, &mut b),
        abi::Return::error(abi::NOT_SUPPORTED)
    );
    let next = create(&mut s, &mut b).unwrap();
    assert_eq!(next.vm, child.vm);
    assert_ne!(next.epoch, child.epoch);
    b.entries[slot] = Some(row(next.vm.0, next.epoch, true));
    assert_eq!(
        s.execute_call(Caller::Guest(next.epoch), call, &mut b),
        abi::Return::error(abi::DENIED)
    );
}
