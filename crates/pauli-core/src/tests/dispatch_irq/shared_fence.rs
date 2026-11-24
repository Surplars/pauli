//! Exercise a sibling's fence on the one physical owner of both off-CPU contexts.
//! Only the dispatch owner borrows CPU state. Fencing is maintenance, not a VS entry.
use super::*;
use crate::arch::{Control, ControlReply};
use crate::tests::model::{KICK_IN_FLIGHT, KICKED_OWNERS, KICKS};
use crate::tests::model_cpu::Cpu;
use crate::vm::slot::VcpuSlot;
use std::{boxed::Box, sync::mpsc, thread, time::Duration};

fn fence(slot: &VcpuSlot<Cpu>, mask: usize, instruction: bool) {
    slot.data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Control(Control::Fence {
            mask,
            base: 0,
            instruction,
        }));
}

fn reset_kicks() {
    KICKED_OWNERS.with(|mask| mask.set(0));
    KICKS.with(|calls| calls.set(0));
}

fn kicks() -> (u64, usize) {
    (
        KICKS.with(|calls| calls.get()),
        KICKED_OWNERS.with(|mask| mask.get()),
    )
}

#[test]
fn failed_inline_drain_returns_the_sibling_for_retirement_and_spares_peer_vm() {
    let f = Fixture::with_vcpus(1, [2, 1]);
    let rt = &f.runtime;
    for id in 0..2 {
        rt.manager.start(rt.board, id).unwrap();
    }
    let (vm, peer) = (rt.manager.vm(0).unwrap(), rt.manager.vm(1).unwrap());
    let (sender, target) = (vm.cpu(0).unwrap(), vm.cpu(1).unwrap());
    target.mailbox.lock().fail_drain.set(true);
    fence(sender, 2, true);
    let mut local = Dispatcher::new();
    rt.dispatch(&mut local).unwrap();
    assert!(vm.lifecycle.stopping() && sender.retired());
    assert!(sender.data.lock().context.as_ref().unwrap().reply.is_none());
    assert!(
        target.data.lock().context.is_some(),
        "borrowed sibling was lost on error"
    );
    assert_eq!(target.mailbox.lock().armed(), 0);

    // Dispatch still owes the abandoned target's fence, but not a wake for the failed sender.
    assert!(rt.dispatch(&mut local).unwrap());
    assert!(!target.mailbox.lock().pending() && vm.retired());
    assert!(vm.try_join(0, rt.manager.link()).unwrap());
    assert_eq!(vm.lifecycle.state(), State::Faulted);
    assert_eq!(peer.lifecycle.state(), State::Ready);
    assert!(
        peer.cpu(0)
            .unwrap()
            .data
            .lock()
            .context
            .as_ref()
            .unwrap()
            .steps
            > 0
    );
}

#[test]
fn synchronizes_a_waiting_sibling_without_waking_or_entering_it() {
    let f = Fixture::new(1, 2);
    let rt = &f.runtime;
    rt.manager.start(rt.board, 0).unwrap();
    let vm = rt.manager.vm(0).unwrap();
    let (target, sender) = (vm.cpu(0).unwrap(), vm.cpu(1).unwrap());
    assert_eq!((target.owner, sender.owner), (0, 0));
    target
        .data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Wait);
    let mut local = Dispatcher::new();
    assert!(rt.dispatch(&mut local).unwrap());
    assert_eq!(target.run_state(), CpuState::Waiting);
    let entry = (vm.config.guest_base + pauli_config::KERNEL_OFFSET) as usize;
    sender.request_start(entry, 0).unwrap();
    fence(sender, 1, false);
    fence(sender, 1, true);
    reset_kicks();

    assert!(rt.dispatch(&mut local).unwrap());
    assert_eq!(kicks(), (0, 0));
    assert_eq!(sender.run_state(), CpuState::Runnable);
    assert_eq!(
        sender.data.lock().context.as_ref().unwrap().reply,
        Some(ControlReply::Success(0))
    );
    let data = target.data.lock();
    let cpu = data.context.as_ref().unwrap();
    assert_eq!(
        (data.state, cpu.steps, cpu.ipis, cpu.fences),
        (CpuState::Waiting, 1, 0, [1, 1])
    );
    assert!(data.pending.is_none() && data.start.is_none() && !target.retired());
    let inbox = target.mailbox.lock();
    assert!(!inbox.pending());
    #[cfg(feature = "selftest")]
    assert_eq!(
        (inbox.ready_count.get(), inbox.ready.get()),
        (2, Some((true, 1, sender.index, false)))
    );
}

#[test]
fn mixed_fanout_leaves_a_siblings_hsm_start_pending_but_waits_for_remote_ack() {
    let f = Fixture::new(2, 3);
    let rt = &f.runtime;
    rt.manager.start(rt.board, 0).unwrap();
    let vm = rt.manager.vm(0).unwrap();
    let (sender, remote, sibling) = (vm.cpu(0).unwrap(), vm.cpu(1).unwrap(), vm.cpu(2).unwrap());
    assert_eq!((sender.owner, remote.owner, sibling.owner), (0, 1, 0));
    let entry = (vm.config.guest_base + pauli_config::KERNEL_OFFSET) as usize;
    sibling.request_start(entry, 37).unwrap();
    fence(sender, 0b110, true);
    reset_kicks();
    let mut local = Dispatcher::new();
    assert!(rt.dispatch(&mut local).unwrap());
    assert_eq!(kicks(), (1, 1 << remote.owner));
    assert_eq!(sender.run_state(), CpuState::Blocked);
    assert!(sender.data.lock().context.as_ref().unwrap().reply.is_none());
    {
        let data = sibling.data.lock();
        let cpu = data.context.as_ref().unwrap();
        assert_eq!(
            (data.state, data.start),
            (CpuState::Starting, Some((entry, 37)))
        );
        assert_eq!((cpu.steps, cpu.fences), (0, [0, 1]));
        assert!(sibling.retired() && data.pending.is_none());
    }
    assert!(!sibling.mailbox.lock().pending());
    assert!(remote.mailbox.lock().pending());
    OWNER.with(|owner| owner.set(remote.owner));
    rt.dispatch(&mut Dispatcher::new()).unwrap();
    assert_eq!(kicks(), (2, 0b11)); // The remote ACK still wakes the sender.
    OWNER.with(|owner| owner.set(sender.owner));
    // Use a fresh cursor: observe the sender before its sibling consumes the HSM start.
    assert!(rt.dispatch(&mut Dispatcher::new()).unwrap());
    assert_eq!(
        sender.data.lock().context.as_ref().unwrap().reply,
        Some(ControlReply::Success(0))
    );
    assert_eq!(sibling.run_state(), CpuState::Starting);
    #[cfg(feature = "selftest")]
    assert_eq!(
        (
            remote.mailbox.lock().ready_count.get(),
            sibling.mailbox.lock().ready_count.get()
        ),
        (0, 0)
    );
}

#[test]
fn inline_drain_preserves_a_concurrent_remote_requesters_completion_kick() {
    let f = Fixture::new(2, 3);
    let rt = &f.runtime;
    rt.manager.start(rt.board, 0).unwrap();
    let vm = rt.manager.vm(0).unwrap();
    let (sender, other, target) = (vm.cpu(0).unwrap(), vm.cpu(1).unwrap(), vm.cpu(2).unwrap());
    let entry = (vm.config.guest_base + pauli_config::KERNEL_OFFSET) as usize;
    other.request_start(entry, 0).unwrap();
    fence(sender, 0b110, true);
    fence(other, 1 << target.index, false);
    reset_kicks();
    const TIMEOUT: Duration = Duration::from_secs(3);
    let (go, sent) = mpsc::channel();
    let (armed, posted) = mpsc::channel();
    thread::scope(|s| {
        s.spawn(move || {
            OWNER.with(|owner| owner.set(other.owner));
            sent.recv_timeout(TIMEOUT).unwrap();
            // Complete the sender's remote ticket, then issue our own sibling ticket
            // while the sender is still in kick. Only the sender can drain that sibling.
            assert!(rt.dispatch(&mut Dispatcher::new()).unwrap());
            assert_eq!(other.run_state(), CpuState::Blocked);
            assert_eq!(target.mailbox.lock().armed(), 1 << other.index);
            armed.send(()).unwrap();
        });
        KICK_IN_FLIGHT.with(|hook| {
            hook.set(Some(Box::new(move |owner| {
                assert_eq!(owner, 1);
                go.send(()).unwrap();
                posted.recv_timeout(TIMEOUT).unwrap();
            })))
        });
        assert!(rt.dispatch(&mut Dispatcher::new()).unwrap());
    });
    // Initial remote kick plus the other waiter's completion, never self-SSIP.
    assert_eq!(kicks(), (2, 1 << other.owner));
    assert_eq!(
        sender.data.lock().context.as_ref().unwrap().reply,
        Some(ControlReply::Success(0))
    );
    assert!(!target.mailbox.lock().pending());
    let cpu = target.data.lock();
    assert_eq!(
        (
            cpu.context.as_ref().unwrap().fences,
            cpu.context.as_ref().unwrap().steps
        ),
        ([1, 1], 0)
    );
    drop(cpu);
    OWNER.with(|owner| owner.set(other.owner));
    assert!(rt.dispatch(&mut Dispatcher::new()).unwrap());
    assert_eq!(
        other.data.lock().context.as_ref().unwrap().reply,
        Some(ControlReply::Success(0))
    );
}
