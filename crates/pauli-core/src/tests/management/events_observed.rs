//! Real grant mutations, console history, management harvests and guest TAKE.
use super::*;
use crate::{
    arch::EventControl::{Mask, Take},
    monitor::Monitor,
};
use pauli_control::{
    ChildName,
    abi::{EVENT_LOG, EVENT_MAIL, EVENT_RESCAN, EVENT_STATE},
};

fn command(f: &Fixture, m: &mut Monitor, line: &[u8]) {
    f.runtime.board.input.lock().extend(line);
    while !f.runtime.board.input.lock().is_empty() {
        m.poll(&f.runtime);
    }
}

#[test]
fn creator_and_inspect_only_peer_observe_a_child_independently() {
    let f = Fixture::with_templates(1, [1; 2], 98, |slot, vm| {
        vm.control_events = true;
        crate::tests::shape::template(slot, vm)
    });
    let rt = &f.runtime;
    let mut b = Target(rt);
    let mut s = Service::new();
    let mut m = Monitor::new();
    b.start(VmId(1)).unwrap();
    b.start(VmId(2)).unwrap();
    m.poll(rt);
    let creator = rt.manager.vm(0).unwrap();
    let other = rt.manager.vm(1).unwrap();
    for vm in [&creator, &other] {
        vm.control_event(Mask(EVENT_LOG | EVENT_STATE), 0).unwrap();
    }
    rt.manager
        .set_budget(0, Some(crate::manager::Budget::from_mib(1, 1, 96)))
        .unwrap();
    s.grant(Caller::Host, creator.epoch(), VmId(2), Rights::CREATE, &b)
        .unwrap();
    let child = s
        .execute(
            Caller::Guest(creator.epoch()),
            Request {
                vm: VmId(2),
                epoch: other.epoch(),
                operation: Operation::Create(ChildName::new("observed").unwrap().into()),
            },
            &mut b,
        )
        .unwrap()
        .info;
    s.grant(Caller::Host, other.epoch(), child.vm, Rights::INSPECT, &b)
        .unwrap();
    b.start(child.vm).unwrap();
    let vm = rt.manager.vm(child.vm.0 as usize - 1).unwrap();
    vm.devices.io.lock().console.access(0, Some(1)).unwrap();
    m.poll(rt);
    assert_eq!(creator.control_event(Take, 0), Ok(EVENT_STATE | EVENT_LOG));
    assert_eq!(other.control_event(Take, 0), Ok(EVENT_STATE));
    vm.devices.io.lock().console.access(0, Some(2)).unwrap();
    s.grant(Caller::Host, other.epoch(), child.vm, Rights::LOG, &b)
        .unwrap();
    m.poll(rt);
    assert_eq!(other.control_event(Take, 0), Ok(EVENT_RESCAN)); // Only its prior INSPECT binding changed.
    assert_eq!(creator.control_event(Take, 0), Ok(EVENT_LOG)); // The other recipient retained the dirty batch.
    vm.devices.io.lock().console.access(0, Some(3)).unwrap();
    m.poll(rt);
    assert_eq!(other.control_event(Take, 0), Ok(EVENT_LOG));
    assert_eq!(creator.control_event(Take, 0), Ok(EVENT_LOG));
}

#[test]
fn log_grant_begins_at_binding_and_batches_history_writes_despite_full_host_tx() {
    let f = Fixture::with_config(1, [1; 2], 0, |_, cfg| cfg.control_events = true);
    let rt = &f.runtime;
    let mut b = Target(rt);
    let mut s = Service::new();
    let mut m = Monitor::new();
    b.start(VmId(1)).unwrap();
    b.start(VmId(2)).unwrap();
    m.poll(rt);
    let watcher = rt.manager.vm(0).unwrap();
    let peer = rt.manager.vm(1).unwrap();
    watcher.control_event(Mask(7), 0).unwrap();
    peer.control_event(Mask(7), 0).unwrap();
    let take = || watcher.control_event(Take, 0).unwrap();
    let append = |n| {
        let mut io = peer.devices.io.lock();
        for _ in 0..n {
            io.console.access(0, Some(65)).unwrap();
        }
    };
    append(1024);
    assert!(peer.devices.io.lock().console.is_full());
    s.grant(Caller::Host, watcher.epoch(), VmId(2), Rights::LOG, &b)
        .unwrap();
    assert_eq!(take(), 0);
    append(10);
    assert_eq!(peer.devices.io.lock().console.dropped, 10);
    assert_eq!(take(), 0);
    m.poll(rt);
    assert_eq!(take(), EVENT_LOG);
    for _ in 0..2 {
        m.poll(rt);
        assert_eq!(take(), 0);
    }
    assert_eq!(peer.control_event(Take, 0).unwrap(), 0);
    assert_eq!(
        s.execute(
            Caller::Guest(watcher.epoch()),
            Request {
                vm: VmId(2),
                epoch: 0,
                operation: Operation::Inspect
            },
            &mut b
        ),
        Err(Error::Denied)
    );

    // Retiring an unpublished batch removes only its source. Once regranted,
    // bytes written in the gap cannot produce a retrospective hint.
    append(1);
    s.revoke(Caller::Host, watcher.epoch(), VmId(2), &b)
        .unwrap();
    append(1);
    s.grant(Caller::Host, watcher.epoch(), VmId(2), Rights::LOG, &b)
        .unwrap();
    m.poll(rt);
    assert_eq!(take(), EVENT_RESCAN);
    append(1);
    m.poll(rt);
    assert_eq!(take(), EVENT_LOG);
    watcher.control_event(Mask(EVENT_MAIL), 0).unwrap();
    s.revoke(Caller::Host, watcher.epoch(), VmId(2), &b)
        .unwrap();
    assert_eq!(take(), 0);
}

#[test]
fn inspect_observes_stop_join_and_new_epoch_while_log_requires_fresh_grant() {
    let f = Fixture::with_config(1, [1; 2], 0, |slot, cfg| cfg.control_events = slot == 0);
    let rt = &f.runtime;
    let mut b = Target(rt);
    let mut m = Monitor::new();
    b.start(VmId(1)).unwrap();
    b.start(VmId(2)).unwrap();
    m.poll(rt);
    let watcher = rt.manager.vm(0).unwrap();
    let peer = rt.manager.vm(1).unwrap();
    let take = || watcher.control_event(Take, 0).unwrap();
    watcher
        .control_event(Mask(EVENT_LOG | EVENT_STATE), 0)
        .unwrap();
    command(&f, &mut m, b"vm grant alpha beta inspect,log\n");
    assert_eq!(take(), 0);
    let mut dispatcher = Dispatcher::new();
    rt.dispatch(&mut dispatcher).unwrap();
    m.poll(rt);
    assert_eq!(take(), 0); // A scheduling slice is not a state change.

    let old = peer.epoch();
    command(&f, &mut m, b"vm stop beta\n");
    assert_eq!(peer.lifecycle.state(), State::Stopping);
    assert_eq!(take(), EVENT_STATE);
    m.poll(rt);
    for _ in 0..2 {
        rt.dispatch(&mut dispatcher).unwrap();
    }
    m.poll(rt);
    assert_eq!(peer.lifecycle.state(), State::Stopped);
    assert_eq!(take(), EVENT_STATE);
    m.poll(rt);
    assert_eq!(take(), 0);
    command(&f, &mut m, b"vm start beta\n");
    assert_ne!(old, peer.epoch());
    assert_eq!(take(), EVENT_RESCAN | EVENT_STATE);
    peer.devices.io.lock().console.access(0, Some(33)).unwrap();
    m.poll(rt);
    assert_eq!(take(), 0);
    command(&f, &mut m, b"vm grant alpha beta log\n");
    assert_eq!(take(), EVENT_RESCAN); // Replacement withdrew only INSPECT.
    peer.devices.io.lock().console.access(0, Some(34)).unwrap();
    m.poll(rt);
    assert_eq!(take(), EVENT_LOG);
}

#[test]
fn inactive_watcher_and_destroyed_subject_do_not_transfer_observations_to_replacements() {
    let f = Fixture::with_config(1, [1; 2], 66, |slot, cfg| cfg.control_events = slot == 0);
    let rt = &f.runtime;
    let mut b = Target(rt);
    let mut m = Monitor::new();
    b.start(VmId(2)).unwrap();
    m.poll(rt);
    let watcher = rt.manager.vm(0).unwrap();
    let peer = rt.manager.vm(1).unwrap();
    let take = || watcher.control_event(Take, 0).unwrap();
    command(&f, &mut m, b"vm grant alpha beta log\n");
    peer.devices.io.lock().console.access(0, Some(1)).unwrap();
    m.poll(rt); // Target epoch exists, but its inactive receiver cannot take a hint.
    command(&f, &mut m, b"vm start alpha\n");
    watcher
        .control_event(Mask(EVENT_LOG | EVENT_STATE), 0)
        .unwrap();
    assert_eq!(take(), 0);
    peer.devices.io.lock().console.access(0, Some(2)).unwrap();
    m.poll(rt);
    assert_eq!(take(), EVENT_LOG);
    let name = ChildName::new("child").unwrap().into();
    let child = b.create(None, VmId(2), name).unwrap();
    command(&f, &mut m, b"vm grant alpha child inspect,log\n");
    assert_eq!(take(), 0);
    command(&f, &mut m, b"vm destroy child\n");
    assert_eq!(take(), EVENT_RESCAN);
    let fresh = b.create(None, VmId(2), name).unwrap();
    assert_eq!(child.vm, fresh.vm);
    assert_ne!(child.epoch, fresh.epoch);
    b.start(fresh.vm).unwrap();
    rt.manager
        .vm(fresh.vm.0 as usize - 1)
        .unwrap()
        .devices
        .io
        .lock()
        .console
        .access(0, Some(3))
        .unwrap();
    m.poll(rt);
    assert_eq!(take(), 0);

    command(&f, &mut m, b"vm stop alpha\n");
    let mut owner = Dispatcher::new();
    for _ in 0..3 {
        rt.dispatch(&mut owner).unwrap();
    }
    m.poll(rt);
    command(&f, &mut m, b"vm start alpha\n");
    watcher
        .control_event(Mask(EVENT_LOG | EVENT_STATE), 0)
        .unwrap();
    peer.devices.io.lock().console.access(0, Some(4)).unwrap();
    m.poll(rt);
    assert_eq!(take(), 0);
}
