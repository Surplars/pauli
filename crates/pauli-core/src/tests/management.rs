//! Management parser and real Service/VM adapter integration tests.
//! Ownership/Lock Ordering: the fixture thread is the sole management owner.
//! Invariants: no unauthorized or stale request mutates a live slot.
use super::model::Fixture;
use crate::{
    monitor::{
        backend::Target,
        commands::{Command, parse},
    },
    runtime::Dispatcher,
    vm::State,
};
use pauli_control::{Backend, Caller, Error, Operation, Request, Rights, Service, VmId};
#[cfg(feature = "guest-control")]
mod boot_grant;
#[cfg(feature = "guest-control")]
mod boot_quota;
#[cfg(feature = "guest-control")]
mod events_mail;
#[cfg(feature = "guest-control")]
mod events_observed;
#[cfg(feature = "guest-control")]
mod mail;
#[cfg(feature = "guest-control")]
mod stats;

#[test]
fn command_grammar_is_exact() {
    assert_eq!(
        parse("vm start alpha --attach"),
        Ok(Command::Start("alpha", true))
    );
    assert_eq!(parse("vm status"), Ok(Command::Status(None)));
    assert_eq!(parse("vm capacity"), Ok(Command::Capacity));
    assert_eq!(parse("vm disks"), Ok(Command::Disks));
    assert_eq!(parse("vm quota alpha"), Ok(Command::Quota("alpha")));
    assert_eq!(
        parse("vm quota alpha clear"),
        Ok(Command::SetQuota("alpha", None))
    );
    assert_eq!(
        parse("vm quota alpha 2 3 128"),
        Ok(Command::SetQuota(
            "alpha",
            Some(crate::manager::Budget::from_mib(2, 3, 128))
        ))
    );
    assert_eq!(parse("vm status 2"), Ok(Command::Status(Some("2"))));
    for line in [
        "shutdown now",
        "vm grant a b none",
        "vm start alpha --attach extra",
        "vm stop",
        "vm quota alpha -1 2 64",
        "vm quota alpha 2 3",
        "vm create a b --vcpus",
        "vm create a b --vcpus 0",
        "vm create a b --vcpus 256",
        "vm create a b --memory-mib -1",
        "vm create a b --memory-mib 96 --memory-mib 128",
        "vm create a b --cores 2",
        "vm create a b --disk 4",
        "vm create a b --disk 0 --disk 0",
        "vm grant a b disk4",
        "1 2 3 4 5 6 7",
    ] {
        assert!(parse(line).is_err());
    }
}

#[test]
fn authorization_precedes_effects_and_epochs_do_not_transfer() {
    let fixture = Fixture::new(1, 1);
    let runtime = &fixture.runtime;
    let mut target = Target(runtime);
    let mut service = Service::new();
    target.start(VmId(1)).unwrap();
    target.start(VmId(2)).unwrap();
    let alpha = runtime.manager.vm(0).unwrap();
    let beta = runtime.manager.vm(1).unwrap();
    let caller = Caller::Guest(alpha.epoch());
    let stop = Request {
        vm: VmId(2),
        epoch: beta.epoch(),
        operation: Operation::Stop,
    };
    assert_eq!(
        service.execute(caller, stop, &mut target),
        Err(Error::Denied)
    );
    assert_eq!(beta.lifecycle.state(), State::Ready);
    service
        .grant(Caller::Host, alpha.epoch(), VmId(2), Rights::STOP, &target)
        .unwrap();
    assert_eq!(
        service.execute(caller, Request { epoch: 0, ..stop }, &mut target),
        Err(Error::StaleEpoch)
    );
    assert!(service.execute(caller, stop, &mut target).unwrap().changed);
    let mut owner = Dispatcher::new();
    runtime.dispatch(&mut owner).unwrap();
    runtime.manager.service(runtime.board);
    target.start(VmId(2)).unwrap();
    assert_ne!(beta.epoch(), stop.epoch);
    assert_eq!(
        service.execute(caller, stop, &mut target),
        Err(Error::StaleEpoch)
    );
    let old_epoch = alpha.epoch();
    alpha.host_stop();
    runtime.dispatch(&mut owner).unwrap();
    runtime.manager.service(runtime.board);
    target.start(VmId(1)).unwrap();
    assert_ne!(alpha.epoch(), old_epoch);
    assert_eq!(
        service.execute(
            Caller::Guest(alpha.epoch()),
            Request {
                epoch: beta.epoch(),
                ..stop
            },
            &mut target
        ),
        Err(Error::Denied)
    );
}

#[cfg(feature = "guest-control")]
#[test]
fn guest_rpc_parks_and_uses_actual_source_epoch() {
    use crate::arch::{Control, ControlReply, ExitKind};
    use crate::vm::slot::CpuState;
    let fixture = Fixture::new(1, 1);
    let runtime = &fixture.runtime;
    runtime.manager.start(runtime.board, 0).unwrap();
    runtime.manager.start(runtime.board, 1).unwrap();
    let vm = runtime.manager.vm(0).unwrap();
    let peer = runtime.manager.vm(1).unwrap();
    let slot = vm.cpu(0).unwrap();
    slot.data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Control(Control::Management {
            args: [pauli_control::abi::VERSION, 2, 2, peer.epoch() as u64, 0, 0],
        }));
    let mut owner = Dispatcher::new();
    runtime.dispatch(&mut owner).unwrap();
    assert_eq!(slot.data.lock().state, CpuState::Blocked);
    crate::monitor::rpc::pump(runtime, &mut Service::new(), true);
    for _ in 0..2 {
        runtime.dispatch(&mut owner).unwrap();
    }
    assert_eq!(
        slot.data.lock().context.as_ref().unwrap().reply,
        Some(ControlReply::Transport {
            error: pauli_control::abi::DENIED,
            value: 0
        })
    );
    assert_eq!(peer.lifecycle.state(), State::Ready);
}

#[cfg(feature = "guest-control")]
#[test]
fn one_hart_manager_needs_a_grant_to_start_and_stop_its_service() {
    use crate::monitor::rpc;
    use pauli_control::{abi, queue::Poll};
    let f = Fixture::new(1, 1);
    let rt = &f.runtime;
    rt.manager.start(rt.board, 0).unwrap();
    let manager = rt.manager.vm(0).unwrap();
    let peer = rt.manager.vm(1).unwrap();
    let mut grants = Service::new();
    let call = |grants: &mut Service, op: u64, epoch: u32| {
        let words = [abi::VERSION, op, 2, epoch.into(), 0, 0];
        let ticket = rpc::submit(rt, 0, 0, manager.epoch(), words).unwrap();
        rpc::pump(rt, grants, true);
        let Poll::Complete(reply) = rpc::poll(rt, ticket) else {
            panic!("missing local reply")
        };
        reply
    };
    let old = peer.epoch();
    assert_eq!(call(&mut grants, 1, old).error, abi::DENIED);
    assert_eq!(peer.lifecycle.state(), State::Stopped);
    grants
        .grant(
            Caller::Host,
            manager.epoch(),
            VmId(2),
            Rights::parse("inspect,start,stop").unwrap(),
            &Target(rt),
        )
        .unwrap();
    assert_eq!(call(&mut grants, 1, old).error, 0);
    let running = peer.epoch(); // First boot may keep its preallocated epoch.
    assert_eq!(peer.lifecycle.state(), State::Ready);
    assert_eq!(call(&mut grants, 2, running).error, 0);
    // A successful stop is not a join: the one physical owner still owes retirement.
    let mut owner = Dispatcher::new();
    for _ in 0..2 {
        rt.dispatch(&mut owner).unwrap();
    }
    rt.manager.service(rt.board);
    assert!(peer.lifecycle.joined());
    assert_eq!(call(&mut grants, 1, running).error, 0);
    assert_ne!(peer.epoch(), running);
    assert_eq!(call(&mut grants, 2, running).error, abi::INVALID_STATE);
    assert_eq!(manager.lifecycle.state(), State::Ready);
}

#[cfg(feature = "guest-control")]
#[test]
fn queued_management_creation_or_start_cannot_reopen_shutdown_admission() {
    use crate::monitor::rpc;
    use pauli_control::{abi, queue::Poll};
    let f = Fixture::with_pool(1, [1, 1], 66);
    let rt = &f.runtime;
    let mut target = Target(rt);
    target.start(VmId(1)).unwrap();
    let source = target.info(VmId(1)).unwrap().epoch;
    let epoch = u64::from(target.info(VmId(2)).unwrap().epoch);
    let mut service = Service::new();
    service
        .grant(
            Caller::Host,
            source,
            VmId(2),
            Rights::CREATE.union(Rights::START).union(Rights::INSPECT),
            &target,
        )
        .unwrap();
    let mut call = |args, admitting| {
        let ticket = rpc::submit(rt, 0, 0, source, args).unwrap();
        rpc::pump(rt, &mut service, admitting);
        let Poll::Complete(reply) = rpc::poll(rt, ticket) else {
            panic!("missing management reply")
        };
        reply
    };
    let create = [1, 3, 2, epoch, u64::from_le_bytes(*b"work\0\0\0\0"), 0];
    for op in [create, [1, 1, 2, epoch, 0, 0]] {
        assert_eq!(call(op, false), abi::Return::error(abi::INVALID_STATE));
    }
    assert_eq!(call([1, 0, 2, 0, 0, 0], false).error, 0);
    assert_eq!(rt.manager.vms().count(), 2);
    assert!(!target.info(VmId(2)).unwrap().active);
    assert_eq!(
        call([1, abi::CONSOLE_READ, 2, epoch, 0, 0], true).error,
        abi::DENIED
    );
    let child = call(create, true);
    assert_eq!(child.error, 0);
    let (id, epoch) = (child.value >> 32, u64::from(child.value as u32));
    assert_eq!(
        call([1, abi::CONSOLE_READ, id, epoch, 0, 0], false),
        abi::Return { error: 0, value: 0 },
        "creation grants the sponsor LOG on the new child's empty console"
    );
    assert_eq!(
        call([1, abi::MAIL_RECV, id, 0, 0, 0], false),
        abi::Return { error: 0, value: 0 },
        "the creator has epoch-bound MAIL authority for its new child"
    );
}
