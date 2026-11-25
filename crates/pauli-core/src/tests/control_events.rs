//! Direct control stays local to the issuer, even while the management owner is idle.
use super::model::{Arch, Fixture, KICKED_OWNERS, KICKS, OWNER, REJECT_KICK_ONCE};
use crate::{
    arch::{Control, ControlReply, EventControl, ExitKind},
    runtime::Dispatcher,
    vm::{Vm, slot::CpuState},
};
use core::sync::atomic::Ordering::Acquire;
use pauli_control::{
    MailMessage, VmId,
    abi::{EVENT_LOG, EVENT_MAIL, EVENT_RESCAN, EVENT_STATE},
};
use std::sync::Barrier;

fn opted(vcpus: u8) -> Fixture {
    let f = Fixture::with_config(2, [vcpus, 1], 0, |slot, vm| vm.control_events = slot == 0);
    for slot in 0..2 {
        f.runtime.manager.start(f.runtime.board, slot).unwrap();
    }
    f
}

// The management policy tests cover reverse-grant checks. Here the event producer
// has authorized this copied message and holds the same I/O lock as production.
fn post(vm: &Vm<Arch>, epoch: u32, tag: u8) {
    let mut io = vm.devices.io.lock();
    io.mailbox
        .send(
            VmId(2),
            epoch,
            MailMessage {
                tag,
                len: 0,
                data: [0; 6],
            },
            true,
            |_, _| true,
        )
        .unwrap();
    vm.devices.refresh_control_event(&mut io);
}

fn call(f: &Fixture, slot: usize, command: EventControl) -> ControlReply {
    let rt = &f.runtime;
    let vm = rt.manager.vm(slot).unwrap();
    let cpu = vm.cpu(0).unwrap();
    OWNER.with(|o| o.set(cpu.owner));
    cpu.data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Control(Control::Events(command)));
    assert!(rt.dispatch(&mut Dispatcher::new()).unwrap());
    let data = cpu.data.lock();
    assert!(data.pending.is_none());
    assert_eq!(data.state, CpuState::Runnable);
    data.context.as_ref().unwrap().reply.unwrap()
}

#[test]
fn self_take_retains_mail_masked_and_never_waits_for_the_manager() {
    let f = opted(1);
    let vm = f.runtime.manager.vm(0).unwrap();
    let peer = f.runtime.manager.vm(1).unwrap();
    let ok = |value| ControlReply::Transport { error: 0, value };
    assert_eq!(
        call(&f, 1, EventControl::Take),
        ControlReply::Transport {
            error: pauli_control::abi::NOT_SUPPORTED,
            value: 0
        }
    );
    assert_eq!(call(&f, 0, EventControl::Take), ok(0));
    post(&vm, peer.epoch(), 7);
    post(&vm, peer.epoch(), 9);
    assert!(!vm.devices.irqs.lock().lines[0]);
    assert_eq!(call(&f, 0, EventControl::Mask(EVENT_MAIL)), ok(0));
    assert!(vm.devices.irqs.lock().lines[0]);
    assert_eq!(call(&f, 0, EventControl::Mask(0)), ok(EVENT_MAIL));
    assert!(!vm.devices.irqs.lock().lines[0]);
    assert_eq!(call(&f, 0, EventControl::Take), ok(EVENT_MAIL));
    assert_eq!(call(&f, 0, EventControl::Take), ok(0));
    let mut io = vm.devices.io.lock();
    for tag in [7, 9] {
        assert_eq!(
            io.mailbox.recv(VmId(2), peer.epoch()).message.unwrap().tag,
            tag
        );
    }
}

#[test]
fn stop_after_guest_entry_rejects_take_and_mask_before_changing_the_latch() {
    struct StopOnExit<'a>(&'a Vm<Arch>);
    impl crate::trace::Tracer for StopOnExit<'_> {
        fn on_vm_exit(&mut self, _: u8, exit: ExitKind, _: u64) {
            if matches!(exit, ExitKind::Control(Control::Events(_))) {
                self.0.host_stop();
            }
        }
    }
    for command in [EventControl::Take, EventControl::Mask(0)] {
        let f = opted(1);
        let vm = f.runtime.manager.vm(0).unwrap();
        let peer = f.runtime.manager.vm(1).unwrap();
        assert_eq!(vm.control_event(EventControl::Mask(EVENT_MAIL), 0), Ok(0));
        post(&vm, peer.epoch(), 19);
        let cpu = vm.cpu(0).unwrap();
        cpu.data
            .lock()
            .context
            .as_mut()
            .unwrap()
            .script
            .push_back(ExitKind::Control(Control::Events(command)));
        let mut owner = Dispatcher::with_tracer(StopOnExit(&vm));
        assert!(f.runtime.dispatch(&mut owner).unwrap());
        assert!(vm.lifecycle.stopping());
        assert_eq!(
            cpu.data.lock().context.as_ref().unwrap().reply,
            Some(ControlReply::Transport {
                error: pauli_control::abi::INVALID_STATE,
                value: 0
            })
        );
        assert!(vm.devices.io.lock().mailbox.has_hint());
        assert!(vm.devices.irqs.lock().lines[0]);
    }
}

#[test]
fn masked_rescan_is_ignored_and_subscribed_invalidation_coalesces_with_mail() {
    let f = opted(1);
    let vm = f.runtime.manager.vm(0).unwrap();
    let peer = f.runtime.manager.vm(1).unwrap();
    let ctl = |command| vm.control_event(command, 0).unwrap();
    vm.devices
        .invalidate_control_event(&mut vm.devices.io.lock(), peer.epoch(), EVENT_MAIL);
    assert_eq!(ctl(EventControl::Take), 0);
    assert_eq!(ctl(EventControl::Mask(EVENT_MAIL)), 0);
    vm.devices
        .invalidate_control_event(&mut vm.devices.io.lock(), peer.epoch(), EVENT_MAIL);
    vm.devices
        .invalidate_control_event(&mut vm.devices.io.lock(), peer.epoch(), EVENT_MAIL);
    assert!(vm.devices.irqs.lock().lines[0]);
    post(&vm, peer.epoch(), 0);
    assert_eq!(ctl(EventControl::Take), EVENT_MAIL | EVENT_RESCAN);
    assert_eq!(ctl(EventControl::Take), 0);
    assert!(!vm.devices.irqs.lock().lines[0]);
    vm.devices
        .invalidate_control_event(&mut vm.devices.io.lock(), peer.epoch(), EVENT_MAIL);
    let old = vm.epoch();
    vm.host_stop();
    f.runtime.manager.service(f.runtime.board);
    f.runtime.manager.start(f.runtime.board, 0).unwrap();
    assert_ne!(vm.epoch(), old);
    assert_eq!(ctl(EventControl::Take), 0);
    assert_eq!(ctl(EventControl::Mask(0)), 0);
}

#[test]
fn withdrawn_origins_and_masked_hints_wake_only_the_interested_classes() {
    let f = opted(1);
    let vm = f.runtime.manager.vm(0).unwrap();
    let ctl = |command| vm.control_event(command, 0).unwrap();
    let publish = |epoch, classes| {
        vm.devices
            .publish_control_event(&mut vm.devices.io.lock(), epoch, classes);
    };
    let withdraw = |epoch, classes| {
        vm.devices
            .invalidate_control_event(&mut vm.devices.io.lock(), epoch, classes);
    };
    let asserted = || vm.devices.irqs.lock().lines[0];

    ctl(EventControl::Mask(EVENT_LOG));
    publish(12, EVENT_STATE);
    assert!(!asserted());
    publish(18, EVENT_LOG);
    assert!(asserted());
    withdraw(18, EVENT_LOG);
    assert_eq!(ctl(EventControl::Take), EVENT_RESCAN | EVENT_STATE);
    assert!(!asserted());

    // A class's sole outstanding source survives an unrelated withdrawal.
    publish(12, EVENT_LOG);
    withdraw(19, EVENT_LOG);
    assert_eq!(ctl(EventControl::Take), EVENT_LOG | EVENT_RESCAN);
    assert_eq!(ctl(EventControl::Mask(EVENT_MAIL)), EVENT_LOG);
    publish(12, EVENT_LOG);
    publish(14, EVENT_LOG);
    withdraw(12, EVENT_LOG);
    assert!(!asserted());
    assert_eq!(ctl(EventControl::Mask(EVENT_LOG)), EVENT_MAIL);
    assert!(asserted());
    assert_eq!(ctl(EventControl::Take), EVENT_RESCAN);
    assert_eq!(ctl(EventControl::Take), 0);
    assert!(!asserted());
}

#[test]
fn rejected_event_kick_retries_only_the_target_owner_and_wakes_its_wfi() {
    let f = opted(2);
    let rt = &f.runtime;
    let vm = rt.manager.vm(0).unwrap();
    let peer = rt.manager.vm(1).unwrap();
    let secondary = vm.cpu(1).unwrap();
    let entry = (vm.config.guest_base + pauli_config::KERNEL_OFFSET) as usize;
    secondary.request_start(entry, 0).unwrap();
    OWNER.with(|o| o.set(secondary.owner));
    let mut worker = Dispatcher::new();
    rt.dispatch(&mut worker).unwrap();
    secondary
        .data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Wait);
    // This owner also serves VM 1; drive one complete round.
    for _ in 0..2 {
        rt.dispatch(&mut worker).unwrap();
    }
    assert_eq!(secondary.run_state(), CpuState::Waiting);
    vm.devices.irqs.lock().control_lane = 1;
    assert_eq!(vm.control_event(EventControl::Mask(EVENT_MAIL), 0), Ok(0));
    OWNER.with(|o| o.set(0));
    KICKS.with(|k| k.set(0));
    KICKED_OWNERS.with(|k| k.set(0));
    REJECT_KICK_ONCE.with(|k| k.set(Some(secondary.owner)));
    post(&vm, peer.epoch(), 1);
    vm.deliver_wakes();
    assert_eq!(vm.devices.wake.load(Acquire), 2);
    assert_eq!(KICKED_OWNERS.with(|k| k.replace(0)), 1 << secondary.owner);
    rt.manager.service(rt.board);
    assert_eq!(vm.devices.wake.load(Acquire), 0);
    assert_eq!(KICKED_OWNERS.with(|k| k.replace(0)), 1 << secondary.owner);
    assert_eq!(KICKS.with(|k| k.get()), 2);
    OWNER.with(|o| o.set(secondary.owner));
    for _ in 0..2 {
        rt.dispatch(&mut worker).unwrap();
    }
    assert_eq!(secondary.run_state(), CpuState::Runnable);
    assert!(secondary.data.lock().context.as_ref().unwrap().external);
    assert!(!peer.devices.irqs.lock().lines[0]);
}

#[test]
fn enqueue_racing_take_has_exactly_one_receipt_and_no_stuck_line() {
    let f = opted(1);
    let vm = f.runtime.manager.vm(0).unwrap();
    let epoch = f.runtime.manager.vm(1).unwrap().epoch();
    vm.control_event(EventControl::Mask(EVENT_MAIL), 0).unwrap();
    for tag in 0..32 {
        let gate = Barrier::new(2);
        let first = std::thread::scope(|s| {
            s.spawn(|| {
                gate.wait();
                post(&vm, epoch, tag);
            });
            gate.wait();
            vm.control_event(EventControl::Take, 0).unwrap()
        });
        let second = vm.control_event(EventControl::Take, 0).unwrap();
        assert!(matches!((first, second), (EVENT_MAIL, 0) | (0, EVENT_MAIL)));
        assert!(!vm.devices.irqs.lock().lines[0]);
        assert_eq!(
            vm.devices
                .io
                .lock()
                .mailbox
                .recv(VmId(2), epoch)
                .message
                .unwrap()
                .tag,
            tag
        );
    }
}
