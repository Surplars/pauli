//! Exercise policy, the production mailbox and the local event latch together.
use super::*;
use crate::arch::EventControl::{Mask, Take};
use pauli_control::{
    ChildName, Info, MailMessage, Reply,
    abi::{EVENT_MAIL, EVENT_RESCAN},
};

const EMPTY: MailMessage = MailMessage {
    tag: 3,
    len: 0,
    data: [0; 6],
};

fn grant(service: &mut Service, backend: &impl Backend, from: Info, to: Info, rights: Rights) {
    service
        .grant(Caller::Host, from.epoch, to.vm, rights, backend)
        .unwrap();
}

fn call(
    service: &mut Service,
    backend: &mut impl Backend,
    from: Info,
    to: Info,
    operation: Operation,
) -> Result<Reply, Error> {
    service.execute(
        Caller::Guest(from.epoch),
        Request {
            vm: to.vm,
            epoch: to.epoch,
            operation,
        },
        backend,
    )
}

#[test]
fn only_successful_mutually_granted_sends_mark_the_inbox() {
    let f = Fixture::with_config(1, [1; 2], 0, |slot, vm| vm.control_events = slot == 1);
    let mut b = Target(&f.runtime);
    let mut s = Service::new();
    b.start(VmId(1)).unwrap();
    b.start(VmId(2)).unwrap();
    let a = b.info(VmId(1)).unwrap();
    let r = b.info(VmId(2)).unwrap();
    let receiver = f.runtime.manager.vm(1).unwrap();
    let events = |op| receiver.control_event(op, 0).unwrap();
    let send = Operation::MailSend(EMPTY);

    assert_eq!(call(&mut s, &mut b, a, r, send), Err(Error::Denied));
    grant(&mut s, &b, a, r, Rights::MAIL);
    call(&mut s, &mut b, a, r, send).unwrap();
    grant(&mut s, &b, r, a, Rights::INSPECT);
    assert_eq!(events(Take), 0);
    grant(&mut s, &b, r, a, Rights::MAIL);
    assert_eq!(events(Mask(EVENT_MAIL)), 0);
    assert_eq!(events(Take), 0); // Grants do not publish old messages.
    assert_eq!(
        call(&mut s, &mut b, r, a, Operation::Inspect),
        Err(Error::Denied)
    );
    assert_eq!(
        call(&mut s, &mut b, r, a, Operation::MailRecv)
            .unwrap()
            .mail
            .unwrap()
            .message,
        Some(EMPTY)
    );
    assert_eq!(events(Mask(0)), EVENT_MAIL);
    for _ in 0..8 {
        call(&mut s, &mut b, a, r, send).unwrap();
    }
    assert_eq!(events(Mask(EVENT_MAIL)), 0);
    assert_eq!(events(Take), EVENT_MAIL); // Eight enqueues coalesced while masked.
    assert_eq!(call(&mut s, &mut b, a, r, send), Err(Error::Busy));
    assert_eq!(events(Take), 0);
    for _ in 0..8 {
        assert_eq!(
            call(&mut s, &mut b, r, a, Operation::MailRecv)
                .unwrap()
                .mail
                .unwrap()
                .message,
            Some(EMPTY)
        );
    }
    call(&mut s, &mut b, a, r, send).unwrap();
    call(&mut s, &mut b, r, a, Operation::MailRecv).unwrap();
    assert_eq!(events(Take), 0); // Dequeuing the last marked record retracts MAIL.

    call(&mut s, &mut b, r, a, send).unwrap();
    let legacy = f.runtime.manager.vm(0).unwrap();
    assert_eq!(
        legacy.control_event(Take, 0),
        Err(pauli_control::abi::NOT_SUPPORTED)
    );
    assert!(!legacy.devices.io.lock().mailbox.has_hint());
}

#[test]
fn loss_of_one_binding_retracts_only_its_hints_and_wakes_an_active_watcher_once() {
    let f = Fixture::with_config(1, [1; 2], 0, |_, vm| vm.control_events = true);
    let mut b = Target(&f.runtime);
    let mut s = Service::new();
    b.start(VmId(1)).unwrap();
    b.start(VmId(2)).unwrap();
    let a = b.info(VmId(1)).unwrap();
    let r = b.info(VmId(2)).unwrap();
    let receiver = f.runtime.manager.vm(1).unwrap();
    let events = |op| receiver.control_event(op, 0).unwrap();
    let send = Operation::MailSend(EMPTY);
    grant(&mut s, &b, r, r, Rights::MAIL);
    grant(&mut s, &b, r, a, Rights::MAIL);
    grant(&mut s, &b, a, r, Rights::MAIL);
    assert_eq!(events(Mask(EVENT_MAIL)), 0);
    call(&mut s, &mut b, r, r, send).unwrap();
    call(&mut s, &mut b, a, r, send).unwrap();
    grant(&mut s, &b, r, a, Rights::MAIL.union(Rights::LOG));
    // A failed guest revoke and a harmless replacement both preserve the binding.
    assert_eq!(
        s.revoke(Caller::Guest(r.epoch), r.epoch, a.vm, &b),
        Err(Error::Denied)
    );
    grant(&mut s, &b, r, a, Rights::LOG);
    assert_eq!(events(Take), EVENT_MAIL | EVENT_RESCAN);
    assert_eq!(events(Take), 0);
    call(&mut s, &mut b, a, r, send).unwrap();
    assert_eq!(events(Take), 0); // Sender still has MAIL but receiver cannot observe it.
    grant(&mut s, &b, r, a, Rights::MAIL);
    for _ in 0..2 {
        assert_eq!(
            call(&mut s, &mut b, r, a, Operation::MailRecv)
                .unwrap()
                .mail
                .unwrap()
                .message,
            Some(EMPTY)
        );
    }
    assert_eq!(events(Take), 0);
    assert_eq!(
        call(&mut s, &mut b, r, r, Operation::MailRecv)
            .unwrap()
            .mail
            .unwrap()
            .message,
        Some(EMPTY)
    );

    call(&mut s, &mut b, a, r, send).unwrap();
    events(Mask(0));
    s.revoke(Caller::Host, a.epoch, r.vm, &b).unwrap();
    events(Mask(EVENT_MAIL));
    assert_eq!(events(Take), 0); // Masked watcher gets neither stale MAIL nor RESCAN.
    grant(&mut s, &b, a, r, Rights::MAIL);
    s.revoke(Caller::Host, a.epoch, r.vm, &b).unwrap();
    assert_eq!(events(Take), EVENT_RESCAN); // Binding loss wakes even with no pending mark.
    s.revoke(Caller::Host, a.epoch, r.vm, &b).unwrap();
    assert_eq!(events(Take), 0);
}

#[test]
fn a_one_way_mail_watcher_alone_is_woken_on_grant_loss_or_peer_retirement() {
    let f = Fixture::with_config(1, [1; 2], 66, |slot, vm| vm.control_events = slot == 1);
    let mut b = Target(&f.runtime);
    let mut s = Service::new();
    b.start(VmId(1)).unwrap();
    b.start(VmId(2)).unwrap();
    let sender = b.info(VmId(1)).unwrap();
    let watcher = b.info(VmId(2)).unwrap();
    let receiver = f.runtime.manager.vm(1).unwrap();
    let events = |op| receiver.control_event(op, 0).unwrap();
    events(Mask(EVENT_MAIL));

    // Only the watcher holds MAIL. It can RECEIVE and wait with no peer sends yet.
    grant(&mut s, &b, watcher, sender, Rights::MAIL);
    assert_eq!(
        call(&mut s, &mut b, watcher, sender, Operation::MailRecv)
            .unwrap()
            .mail
            .unwrap()
            .message,
        None
    );
    s.revoke(Caller::Host, watcher.epoch, sender.vm, &b)
        .unwrap();
    assert_eq!(events(Take), EVENT_RESCAN);
    grant(&mut s, &b, watcher, sender, Rights::MAIL);
    grant(&mut s, &b, watcher, sender, Rights::LOG);
    assert_eq!(events(Take), EVENT_RESCAN);
    grant(&mut s, &b, watcher, sender, Rights::MAIL);
    s.retire(sender.epoch, &b);
    assert_eq!(events(Take), EVENT_RESCAN);
    s.retire(sender.epoch, &b);
    assert_eq!(events(Take), 0);

    // In the opposite direction, the enabled receiver is now only the target
    // of MAIL. A one-way grant's revocation must reveal nothing to that target.
    s.revoke(Caller::Host, watcher.epoch, sender.vm, &b)
        .unwrap();
    grant(&mut s, &b, sender, watcher, Rights::MAIL);
    call(&mut s, &mut b, sender, watcher, Operation::MailSend(EMPTY)).unwrap();
    s.revoke(Caller::Host, sender.epoch, watcher.vm, &b)
        .unwrap();
    assert_eq!(events(Take), 0);

    // A stopped template clone may be destroyed without a prior stop notice.
    let child = b
        .create(None, sender.vm, ChildName::new("child").unwrap().into())
        .unwrap();
    grant(&mut s, &b, watcher, child, Rights::MAIL);
    s.execute(
        Caller::Host,
        Request {
            vm: child.vm,
            epoch: child.epoch,
            operation: Operation::Destroy,
        },
        &mut b,
    )
    .unwrap();
    assert_eq!(events(Take), EVENT_RESCAN);
    assert_eq!(events(Take), 0);
}

#[test]
fn stopped_destroyed_and_reused_slot_leaves_no_notification_authority() {
    let f = Fixture::with_config(1, [1; 2], 66, |slot, vm| vm.control_events = slot == 1);
    let rt = &f.runtime;
    let mut b = Target(rt);
    let mut s = Service::new();
    b.start(VmId(1)).unwrap();
    b.start(VmId(2)).unwrap();
    let r = b.info(VmId(2)).unwrap();
    let receiver = rt.manager.vm(1).unwrap();
    let events = |op| receiver.control_event(op, 0).unwrap();
    let spec = ChildName::new("child").unwrap().into();
    let child = b.create(None, VmId(1), spec).unwrap();
    b.start(child.vm).unwrap();
    let child = b.info(child.vm).unwrap();
    grant(&mut s, &b, r, child, Rights::MAIL);
    grant(&mut s, &b, child, r, Rights::MAIL);
    events(Mask(EVENT_MAIL));
    call(&mut s, &mut b, child, r, Operation::MailSend(EMPTY)).unwrap();
    let host = |s: &mut Service, b: &mut _, op| {
        s.execute(
            Caller::Host,
            Request {
                vm: child.vm,
                epoch: child.epoch,
                operation: op,
            },
            b,
        )
    };
    assert_eq!(host(&mut s, &mut b, Operation::Destroy), Err(Error::Busy));
    assert_eq!(events(Take), EVENT_MAIL); // A failed destroy preserves the binding.
    host(&mut s, &mut b, Operation::Stop).unwrap();
    assert_eq!(events(Take), EVENT_RESCAN);
    assert_eq!(
        call(&mut s, &mut b, r, child, Operation::MailRecv)
            .unwrap()
            .mail
            .unwrap()
            .message,
        Some(EMPTY)
    );
    let mut owner = Dispatcher::new();
    for _ in 0..3 {
        rt.dispatch(&mut owner).unwrap();
    }
    rt.manager.service(rt.board);
    // The host can provision a stopped source before destruction. Its exact
    // handle still has to be retired when the slot is finally returned.
    grant(&mut s, &b, child, r, Rights::MAIL);
    host(&mut s, &mut b, Operation::Destroy).unwrap();
    assert_eq!(events(Take), EVENT_RESCAN);
    let fresh = b.create(None, VmId(1), spec).unwrap();
    assert_eq!(fresh.vm, child.vm);
    assert_ne!(fresh.epoch, child.epoch);
    b.start(fresh.vm).unwrap();
    let fresh = b.info(fresh.vm).unwrap();
    grant(&mut s, &b, fresh, r, Rights::MAIL);
    call(&mut s, &mut b, fresh, r, Operation::MailSend(EMPTY)).unwrap();
    assert_eq!(events(Take), 0);
    assert_eq!(
        call(&mut s, &mut b, r, fresh, Operation::MailRecv),
        Err(Error::Denied)
    );
}
