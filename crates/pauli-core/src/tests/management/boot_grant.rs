//! Drive the real RPC queue before and after boot-domain epoch changes. No
//! console command or guest-supplied identity establishes the initial grant.
use super::Fixture;
use crate::{
    monitor::{Monitor, rpc},
    runtime::Dispatcher,
    vm::State,
};
use pauli_control::{abi, queue::Poll};

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
        request(&f, &mut monitor, 0, 2, 2, old_peer).error,
        abi::INVALID_STATE
    );

    // Local revocation must not be undone by every poll within this manager epoch.
    rt.board.input.lock().extend(b"vm revoke manager service\n");
    monitor.poll(rt);
    assert_eq!(request(&f, &mut monitor, 0, 0, 2, 0).error, abi::DENIED);

    let old_manager = manager.epoch();
    let words = [abi::VERSION, 2, 2, u64::from(peer.epoch()), 0, 0];
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
    assert_eq!(request(&f, &mut monitor, 0, 2, 2, peer.epoch()).error, 0);
}
