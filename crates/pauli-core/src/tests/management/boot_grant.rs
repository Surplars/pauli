//! Drive the real RPC queue before and after boot-domain epoch changes. No
//! console command or guest-supplied identity establishes the initial grant.
use super::Fixture;
use crate::{
    monitor::{Monitor, backend::Target, rpc},
    runtime::Dispatcher,
    vm::State,
};
use pauli_control::{Backend, Caller, Error, Rights, Service, VmId, abi, queue::Poll};

fn request(
    f: &Fixture,
    monitor: &mut Monitor,
    source: usize,
    op: u64,
    target: u64,
    epoch: u32,
) -> abi::Return {
    let rt = &f.runtime;
    let source_epoch = rt.manager.vm(source).unwrap().epoch();
    let words = [abi::VERSION, op, target, epoch.into(), 0, 0];
    let ticket = rpc::submit(rt, source, 0, source_epoch, words).unwrap();
    monitor.poll(rt);
    let Poll::Complete(reply) = rpc::poll(rt, ticket) else {
        panic!("RPC was not completed by the management owner");
    };
    reply
}

fn domains() -> Fixture {
    Fixture::with_config(1, [1, 1], 0, |slot, vm| {
        *vm = vm.renamed(["manager", "service"][slot]).unwrap();
    })
}

#[test]
fn extension_discovery_and_boot_names_do_not_confer_authority() {
    use crate::arch::{Control, ControlReply, ExitKind};
    for (authorized, expected) in [(false, abi::DENIED), (true, 0)] {
        let f = domains();
        let rt = &f.runtime;
        rt.board
            .boot_service
            .store(authorized, core::sync::atomic::Ordering::Relaxed);
        rt.manager.start(rt.board, 0).unwrap();
        let vm = rt.manager.vm(0).unwrap();
        let cpu = vm.cpu(0).unwrap();
        let mut monitor = Monitor::new();
        monitor.poll(rt);
        cpu.data
            .lock()
            .context
            .as_mut()
            .unwrap()
            .script
            .push_back(ExitKind::Control(Control::ManagementProbe));
        rt.dispatch(&mut Dispatcher::new()).unwrap();
        assert_eq!(
            cpu.data.lock().context.as_ref().unwrap().reply,
            Some(ControlReply::Transport {
                error: 0,
                value: abi::probe(rt.manager.capacity() as u32),
            })
        );
        assert_eq!(request(&f, &mut monitor, 0, 0, 2, 0).error, expected);
        assert_eq!(rt.manager.vm(1).unwrap().lifecycle.state(), State::Stopped);
    }
}

#[test]
fn board_grant_is_narrow_and_refreshes_only_for_a_new_manager() {
    for queued_op in [2, abi::CONSOLE_READ, abi::MAIL_SEND, abi::MAIL_RECV] {
        check_manager_restart(queued_op);
    }
}

fn check_manager_restart(queued_op: u64) {
    let f = domains();
    let rt = &f.runtime;
    rt.board
        .boot_service
        .store(true, core::sync::atomic::Ordering::Relaxed);
    let manager = rt.manager.vm(0).unwrap();
    let peer = rt.manager.vm(1).unwrap();
    let mut monitor = Monitor::new();
    // A cold boot may publish Stopped before the first Ready with the same epoch.
    monitor.poll(rt);
    rt.manager.start(rt.board, 0).unwrap();
    let info = request(&f, &mut monitor, 0, 0, 2, 0);
    assert_eq!((info.error, info.value as u32), (0, peer.epoch()));
    assert_eq!(request(&f, &mut monitor, 0, 1, 2, peer.epoch()).error, 0);
    assert_eq!(peer.lifecycle.state(), State::Ready);
    assert_eq!(request(&f, &mut monitor, 1, 0, 1, 0).error, abi::DENIED);
    assert_eq!(
        request(&f, &mut monitor, 0, 4, 2, peer.epoch()).error,
        abi::DENIED
    );

    let old_peer = peer.epoch();
    assert_eq!(
        request(&f, &mut monitor, 0, abi::CONSOLE_READ, 2, old_peer),
        abi::Return { error: 0, value: 0 }
    );
    assert_eq!(
        request(&f, &mut monitor, 1, abi::CONSOLE_READ, 1, manager.epoch()).error,
        abi::DENIED
    );
    assert_eq!(request(&f, &mut monitor, 0, 2, 2, old_peer).error, 0);
    let mut owner = Dispatcher::new();
    for _ in 0..2 {
        rt.dispatch(&mut owner).unwrap();
    }
    rt.manager.service(rt.board);
    assert!(peer.lifecycle.joined());
    assert_eq!(request(&f, &mut monitor, 0, 1, 2, old_peer).error, 0);
    assert_ne!(peer.epoch(), old_peer);
    assert_eq!(
        request(&f, &mut monitor, 0, abi::CONSOLE_READ, 2, old_peer).error,
        abi::INVALID_STATE
    );
    assert_eq!(
        request(&f, &mut monitor, 0, abi::CONSOLE_READ, 2, peer.epoch()).error,
        abi::DENIED
    );
    assert_eq!(
        request(&f, &mut monitor, 0, 2, 2, old_peer).error,
        abi::INVALID_STATE
    );

    // Local revocation must not be undone by every poll within this manager epoch.
    rt.board.input.lock().extend(b"vm revoke manager service\n");
    monitor.poll(rt);
    assert_eq!(request(&f, &mut monitor, 0, 0, 2, 0).error, abi::DENIED);

    let old_manager = manager.epoch();
    let words = [abi::VERSION, queued_op, 2, u64::from(peer.epoch()), 0, 0];
    let stale = rpc::submit(rt, 0, 0, old_manager, words).unwrap();
    manager.host_stop();
    for _ in 0..2 {
        rt.dispatch(&mut owner).unwrap();
    }
    rt.manager.service(rt.board);
    assert!(manager.lifecycle.joined());
    rt.manager.start(rt.board, 0).unwrap();
    assert_ne!(manager.epoch(), old_manager);
    monitor.poll(rt);
    assert_eq!(
        rpc::poll(rt, stale),
        Poll::Complete(abi::Return::error(abi::DENIED))
    );
    assert_eq!(peer.lifecycle.state(), State::Ready);
    assert_eq!(request(&f, &mut monitor, 0, 0, 2, 0).error, 0);
    assert_eq!(
        request(&f, &mut monitor, 0, abi::CONSOLE_READ, 2, peer.epoch()),
        abi::Return { error: 0, value: 0 }
    );
    assert_eq!(request(&f, &mut monitor, 0, 2, 2, peer.epoch()).error, 0);
}

#[test]
fn log_remains_readable_while_stopping_and_after_join_until_cold_restart() {
    for fault in [false, true] {
        let f = domains();
        let rt = &f.runtime;
        rt.manager.start(rt.board, 0).unwrap();
        rt.manager.start(rt.board, 1).unwrap();
        let source = rt.manager.vm(0).unwrap().epoch();
        let peer = rt.manager.vm(1).unwrap();
        let old = peer.epoch();
        let target = Target(rt);
        let mut grants = Service::new();
        let read = |grants: &mut Service, epoch: u32, cursor| {
            let words = [abi::VERSION, abi::CONSOLE_READ, 2, epoch.into(), cursor, 0];
            let ticket = rpc::submit(rt, 0, 0, source, words).unwrap();
            rpc::pump(rt, grants, true);
            let Poll::Complete(reply) = rpc::poll(rt, ticket) else {
                panic!("missing log reply")
            };
            reply
        };
        grants
            .grant(Caller::Host, source, VmId(2), Rights::ALL, &target)
            .unwrap();
        assert_eq!(read(&mut grants, old, 0), abi::Return::error(abi::DENIED));
        grants
            .grant(Caller::Host, source, VmId(2), Rights::LOG, &target)
            .unwrap();
        let payload = [0, 255, 0x1d, b'\r', b'\n', 0x1e];
        let mut io = peer.devices.io.lock();
        for byte in payload {
            io.console.access(0, Some(usize::from(byte))).unwrap();
        }
        drop(io);
        let expected = abi::Return {
            error: 0,
            value: (6 << 48) | u64::from_le_bytes([0, 255, 0x1d, b'\r', b'\n', 0x1e, 0, 0]),
        };
        assert_eq!(read(&mut grants, old, 0), expected);
        assert_eq!(
            read(&mut grants, old, 6),
            abi::Return { error: 0, value: 0 }
        );
        assert_eq!(
            read(&mut grants, old, 7),
            abi::Return::error(abi::INVALID_PARAM)
        );
        peer.request_stop(fault, false);
        assert_eq!(peer.lifecycle.state(), State::Stopping);
        assert_eq!(read(&mut grants, old, 0), expected);
        let mut owner = Dispatcher::new();
        for _ in 0..2 {
            rt.dispatch(&mut owner).unwrap();
        }
        rt.manager.service(rt.board);
        assert!(peer.lifecycle.joined());
        assert_eq!(
            peer.lifecycle.state(),
            if fault {
                State::Faulted
            } else {
                State::Stopped
            }
        );
        assert_eq!(read(&mut grants, old, 0), expected);
        rt.manager.start(rt.board, 1).unwrap();
        let new = peer.epoch();
        assert_ne!(new, old);
        assert_eq!(target.console_read(VmId(2), old, 0), Err(Error::StaleEpoch));
        assert_eq!(
            read(&mut grants, old, 0),
            abi::Return::error(abi::INVALID_STATE)
        );
        assert_eq!(read(&mut grants, new, 0), abi::Return::error(abi::DENIED));
        grants
            .grant(Caller::Host, source, VmId(2), Rights::LOG, &target)
            .unwrap();
        assert_eq!(
            read(&mut grants, new, 0),
            abi::Return { error: 0, value: 0 }
        );
        assert_eq!(
            read(&mut grants, new, 1),
            abi::Return::error(abi::INVALID_PARAM)
        );
    }
}
