//! Real cross-thread admission to the destination mailbox during a modeled native kick.
//! The sender hands off once; its bounded wait always releases the sender on regression.
use super::*;
use crate::arch::{Control, ControlReply};
use crate::tests::model::KICK_IN_FLIGHT;
use std::{boxed::Box, cell::Cell, rc::Rc, sync::mpsc, thread, time::Duration};

#[test]
fn target_receives_guest_ipi_before_the_senders_native_kick_returns() {
    receive_during_kick(Control::Ipi { mask: 1, base: 0 });
}

#[test]
fn target_completes_rfence_before_the_senders_native_kick_returns() {
    receive_during_kick(Control::Fence {
        mask: 1,
        base: 0,
        instruction: true,
    });
}

fn receive_during_kick(action: Control) {
    const LIMIT: Duration = Duration::from_secs(3);
    let f = Fixture::new(2, 2);
    let rt = &f.runtime;
    rt.manager.start(rt.board, 0).unwrap();
    let vm = rt.manager.vm(0).unwrap();
    let (target, sender) = (vm.cpu(0).unwrap(), vm.cpu(1).unwrap());
    assert_eq!((target.owner, sender.owner), (0, 1));
    target
        .data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Wait);
    assert!(rt.dispatch(&mut Dispatcher::new()).unwrap());
    assert_eq!(target.run_state(), CpuState::Waiting);
    let entry = vm.config.guest_base as usize + pauli_config::KERNEL_OFFSET as usize;
    sender.request_start(entry, 0).unwrap();
    sender
        .data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Control(action));
    let ipi = matches!(action, Control::Ipi { .. });

    let (post, posted) = mpsc::channel();
    let (ack, received) = mpsc::channel();
    let early = Rc::new(Cell::new(false));
    thread::scope(|scope| {
        scope.spawn(move || {
            OWNER.with(|o| o.set(target.owner));
            assert_eq!(posted.recv_timeout(LIMIT).unwrap(), target.owner);
            assert!(target.mailbox.lock().pending());
            // RFENCE is host-only maintenance, whereas a guest IPI wakes the parked CPU.
            assert_eq!(rt.dispatch(&mut Dispatcher::new()).unwrap(), ipi);
            assert!(!target.mailbox.lock().pending());
            assert_eq!(
                target.data.lock().context.as_ref().unwrap().ipis,
                u64::from(ipi)
            );
            let _ = ack.send(()); // A timed-out sender has already left the hook.
        });
        OWNER.with(|o| o.set(sender.owner));
        let early = Rc::clone(&early);
        KICK_IN_FLIGHT.with(|h| {
            h.set(Some(Box::new(move |owner| {
                post.send(owner).unwrap();
                early.set(received.recv_timeout(LIMIT).is_ok());
            })))
        });
        assert!(rt.dispatch(&mut Dispatcher::new()).unwrap());
    });
    assert!(
        early.get(),
        "target could not drain its mailbox before kick returned"
    );
    let data = sender.data.lock();
    assert_eq!(data.state, CpuState::Runnable);
    assert_eq!(
        data.context.as_ref().unwrap().reply,
        Some(ControlReply::Success(0))
    );
    if !ipi {
        let inbox = target.mailbox.lock();
        assert_eq!((inbox.disarms.get(), inbox.armed()), (0, 0));
        #[cfg(feature = "selftest")]
        assert_eq!(
            (inbox.ready_count.get(), inbox.ready.get()),
            (1, Some((true, 1, sender.index, false)))
        );
    }
}
