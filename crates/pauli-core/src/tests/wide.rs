//! Exercise wide guest CPU masks with two pinned physical owners and a live peer.
//! Ownership/Lock Ordering: dispatch only the selected model owner; no native CSRs.
//! Invariants: highest-numbered CPU obeys the same ACK, join and cold-start rules as CPU 0.
use super::model::{Arch, Fixture, KICKED_OWNERS, OWNER, Timer};
use crate::{
    arch::{Control, ControlError, ControlReply, ExitKind, Mailbox},
    runtime::Dispatcher,
    vm::{State, Vm, slot::CpuState},
};

fn queue(vm: &Vm<Arch>, id: usize, exit: ExitKind) {
    vm.cpu(id)
        .unwrap()
        .data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(exit);
}
fn observe(vm: &Vm<Arch>, id: usize) -> (u64, u64, Option<ControlReply>) {
    let data = vm.cpu(id).unwrap().data.lock();
    let cpu = data.context.as_ref().unwrap();
    (cpu.steps, cpu.ipis, cpu.reply)
}

#[test]
fn partial_rfence_kick_failure_cancels_all_completion_wakes() {
    use super::model::REJECT_KICK_ONCE;
    let f = Fixture::with_vcpus(3, [3, 1]);
    let rt = &f.runtime;
    let vm = rt.manager.vm(0).unwrap();
    let peer = rt.manager.vm(1).unwrap();
    for slot in [0, 1] {
        rt.manager.start(rt.board, slot).unwrap();
    }
    let peer_epoch = peer.epoch();
    let targets = [vm.cpu(1).unwrap(), vm.cpu(2).unwrap()];
    assert_eq!(targets.map(|s| s.owner), [1, 2]);
    let mut owners = [Dispatcher::new(), Dispatcher::new(), Dispatcher::new()];
    let entry = vm.config.guest_base as usize + pauli_config::KERNEL_OFFSET as usize;
    for target in targets {
        target.request_start(entry, 0).unwrap();
        OWNER.with(|o| o.set(target.owner));
        rt.dispatch(&mut owners[target.owner]).unwrap();
    }

    // Target 1 is notified, target 2's doorbell fails. Neither has run its
    // fence yet; the sender must not receive a successful completion.
    OWNER.with(|o| o.set(0));
    REJECT_KICK_ONCE.with(|v| v.set(Some(2)));
    queue(
        &vm,
        0,
        ExitKind::Control(Control::Fence {
            base: 1,
            mask: 3,
            instruction: false,
        }),
    );
    rt.dispatch(&mut owners[0]).unwrap();
    assert!(vm.lifecycle.stopping());
    assert_eq!(observe(&vm, 0).2, None);
    assert!(targets.iter().all(|t| t.mailbox.lock().pending()));

    // Published work is retained for each target to drain during retirement,
    // but neither completion may kick the cancelled requester (owner zero).
    KICKED_OWNERS.with(|k| k.set(0));
    for target in targets {
        OWNER.with(|o| o.set(target.owner));
        rt.dispatch(&mut owners[target.owner]).unwrap();
        assert!(!target.mailbox.lock().pending());
        assert!(target.retired());
    }
    assert_eq!(KICKED_OWNERS.with(|k| k.get()) & 1, 0);
    OWNER.with(|o| o.set(0));
    rt.manager.service(rt.board);
    assert_eq!(vm.lifecycle.state(), State::Faulted);
    assert_eq!(
        (peer.lifecycle.state(), peer.epoch()),
        (State::Ready, peer_epoch)
    );
}

fn dispatch(f: &Fixture, locals: &mut [Dispatcher<Timer>; 2], owner: usize, rounds: usize) {
    OWNER.with(|o| o.set(owner));
    for _ in 0..rounds {
        f.runtime.dispatch(&mut locals[owner]).unwrap();
    }
}
fn boot(f: &Fixture, locals: &mut [Dispatcher<Timer>; 2]) {
    let rt = &f.runtime;
    let vm = rt.manager.vm(0).unwrap();
    for id in [0, 1] {
        rt.manager.start(rt.board, id).unwrap();
    }
    let entry = vm.config.guest_base as usize + pauli_config::KERNEL_OFFSET as usize;
    // Get every secondary through the guest HSM path, not by marking test slots runnable.
    for cpu in 1..16 {
        queue(
            &vm,
            0,
            ExitKind::Control(Control::Start {
                cpu,
                entry,
                opaque: cpu,
            }),
        );
    }
    for _ in 0..64 {
        dispatch(f, locals, 0, 1);
        dispatch(f, locals, 1, 1);
    }
    assert!((0..16).all(|cpu| observe(&vm, cpu).0 != 0));
    assert!(observe(&rt.manager.vm(1).unwrap(), 0).0 != 0);
}

#[test]
fn high_ipi_and_remote_fence_use_virtual_not_physical_bits() {
    let f = Fixture::with_vcpus(2, [16, 1]);
    let mut locals = [Dispatcher::new(), Dispatcher::new()];
    boot(&f, &mut locals);
    let vm = f.runtime.manager.vm(0).unwrap();
    let peer = f.runtime.manager.vm(1).unwrap();
    for (mask, reply) in [
        (1, ControlReply::Success(0)),
        (3, ControlReply::Error(ControlError::InvalidParameter)),
    ] {
        queue(&vm, 0, ExitKind::Control(Control::Ipi { mask, base: 15 }));
        dispatch(&f, &mut locals, 0, 10);
        dispatch(&f, &mut locals, 1, 10);
        assert_eq!(observe(&vm, 0).2, Some(reply));
        assert_eq!(observe(&vm, 15).1, 1); // An invalid two-bit range has no partial effect.
        assert_eq!(observe(&peer, 0).1, 0);
    }
    queue(
        &vm,
        15,
        ExitKind::Control(Control::Fence {
            mask: 1,
            base: 0,
            instruction: true,
        }),
    );
    dispatch(&f, &mut locals, 1, 12);
    let high = vm.cpu(15).unwrap();
    assert_eq!(high.data.lock().state, CpuState::Blocked);
    KICKED_OWNERS.with(|k| k.set(0));
    dispatch(&f, &mut locals, 0, 1);
    assert_eq!(KICKED_OWNERS.with(|k| k.get()), 1 << high.owner);
    dispatch(&f, &mut locals, 1, 12);
    assert_eq!(high.data.lock().state, CpuState::Runnable);
    assert_eq!(observe(&vm, 15).2, Some(ControlReply::Success(0)));
}

#[test]
fn highest_owner_must_retire_before_restart_and_peer_keeps_running() {
    let f = Fixture::with_vcpus(2, [16, 1]);
    let rt = &f.runtime;
    let mut locals = [Dispatcher::new(), Dispatcher::new()];
    boot(&f, &mut locals);
    let vm = rt.manager.vm(0).unwrap();
    let peer = rt.manager.vm(1).unwrap();
    let (old, other) = (vm.epoch(), peer.epoch());
    let steps = observe(&peer, 0).0;
    queue(&vm, 15, ExitKind::Fault);
    dispatch(&f, &mut locals, 1, 18);
    assert!(vm.lifecycle.stopping());
    assert!(!vm.try_join(0, rt.manager.link()).unwrap());
    dispatch(&f, &mut locals, 0, 1);
    assert!(vm.try_join(0, rt.manager.link()).unwrap());
    assert_eq!(vm.lifecycle.state(), State::Faulted);
    rt.manager.start(rt.board, 0).unwrap();
    assert_ne!(vm.epoch(), old);
    assert!(
        vm.cpus()[1..]
            .iter()
            .all(|slot| slot.data.lock().state == CpuState::Stopped)
    );
    assert!(!vm.cpu(15).unwrap().mailbox.lock().pending());
    assert_eq!(observe(&vm, 15), (0, 0, None));
    let entry = vm.config.guest_base as usize + pauli_config::KERNEL_OFFSET as usize;
    queue(
        &vm,
        0,
        ExitKind::Control(Control::Start {
            cpu: 15,
            entry,
            opaque: 0,
        }),
    );
    dispatch(&f, &mut locals, 0, 2);
    dispatch(&f, &mut locals, 1, 4);
    assert!(observe(&vm, 15).0 > 0);
    assert_eq!(peer.epoch(), other);
    assert!(observe(&peer, 0).0 > steps);
}
