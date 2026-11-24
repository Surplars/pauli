//! All remote tickets must complete; cancellation revokes wakes, not fence obligations.
//! Each target belongs to a different modeled owner, so no drain races the assertions.
use super::*;
use crate::arch::{Control, ControlReply};
use crate::tests::model::{KICKED_OWNERS, REJECT_KICK_ONCE};

fn queue_fanout(vm: &Vm<Arch>) {
    let sender = vm.cpu(0).unwrap();
    assert_eq!([0, 1, 2].map(|i| vm.cpu(i).unwrap().owner), [0, 1, 2]);
    sender
        .data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Control(Control::Fence {
            mask: 0b110,
            base: 0,
            instruction: true,
        }));
}

#[test]
fn fanout_needs_both_acks_and_neither_target_is_a_single_ticket_sample() {
    let f = Fixture::new(3, 3);
    let rt = &f.runtime;
    rt.manager.start(rt.board, 0).unwrap();
    let vm = rt.manager.vm(0).unwrap();
    let sender = vm.cpu(0).unwrap();
    queue_fanout(&vm);
    let mut local = Dispatcher::new();
    assert!(rt.dispatch(&mut local).unwrap());
    for target in 1..=2 {
        assert_eq!(sender.run_state(), CpuState::Blocked);
        assert!(sender.data.lock().context.as_ref().unwrap().reply.is_none());
        OWNER.with(|owner| owner.set(target));
        rt.dispatch(&mut Dispatcher::new()).unwrap();
        OWNER.with(|owner| owner.set(0));
        assert_eq!(rt.dispatch(&mut local).unwrap(), target == 2);
    }
    assert_eq!(sender.run_state(), CpuState::Runnable);
    assert_eq!(
        sender.data.lock().context.as_ref().unwrap().reply,
        Some(ControlReply::Success(0))
    );
    for target in &vm.cpus()[1..] {
        let inbox = target.mailbox.lock();
        assert_eq!((inbox.armed(), inbox.disarms.get()), (0, 0));
        #[cfg(feature = "selftest")]
        assert_eq!(inbox.ready_count.get(), 0);
    }
}

#[test]
fn stop_and_partial_kick_failure_cancel_only_armed_wakes() {
    for reject_last in [false, true] {
        let f = Fixture::new(3, 3);
        let rt = &f.runtime;
        rt.manager.start(rt.board, 0).unwrap();
        let vm = rt.manager.vm(0).unwrap();
        let sender = vm.cpu(0).unwrap();
        let targets = [vm.cpu(1).unwrap(), vm.cpu(2).unwrap()];
        queue_fanout(&vm);
        if reject_last {
            // Publication to the first target succeeds. Failure notifying the
            // second must revoke registrations on BOTH, not just on the last.
            REJECT_KICK_ONCE.with(|next| next.set(Some(targets[1].owner)));
        }
        let mut local = Dispatcher::new();
        rt.dispatch(&mut local).unwrap();
        if !reject_last {
            assert_eq!(sender.run_state(), CpuState::Blocked);
            for target in &targets {
                assert_eq!(target.mailbox.lock().armed(), 1 << sender.index);
            }
            vm.host_stop();
            rt.dispatch(&mut local).unwrap();
        }
        assert!(vm.lifecycle.stopping());
        assert!(sender.data.lock().context.as_ref().unwrap().reply.is_none());
        KICKED_OWNERS.with(|bits| bits.set(0));
        for target in targets {
            {
                let inbox = target.mailbox.lock();
                assert_eq!((inbox.disarms.get(), inbox.armed()), (1, 0));
                assert!(inbox.pending(), "cancellation erased a published fence");
                #[cfg(feature = "selftest")]
                assert_eq!(inbox.ready_count.get(), 0);
            }
            OWNER.with(|owner| owner.set(target.owner));
            rt.dispatch(&mut Dispatcher::new()).unwrap();
            assert!(!target.mailbox.lock().pending());
        }
        assert_eq!(KICKED_OWNERS.with(|bits| bits.get()), 0, "orphan wake");
        assert!(vm.try_join(0, rt.manager.link()).unwrap());
        assert_eq!(
            vm.lifecycle.state(),
            if reject_last {
                State::Faulted
            } else {
                State::Stopped
            }
        );
    }
}
