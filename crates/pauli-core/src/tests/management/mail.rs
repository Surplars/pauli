//! Exercise the copied inbox through the real guest RPC and VM lifecycle.
use super::Fixture;
use crate::{
    monitor::{Monitor, backend::Target, rpc},
    runtime::Dispatcher,
    vm::State,
};
use pauli_control::{Backend, ChildName, Error, MailChunk, MailMessage, VmId, abi, queue::Poll};

#[test]
fn mail_completions_are_counted_once_on_the_issuing_owner() {
    use crate::{
        arch::{Control, ControlReply, ExitKind},
        tests::model::OWNER,
        trace::{HsStats, StatsTracer},
        vm::slot::CpuState,
    };
    use core::sync::atomic::Ordering::Relaxed;
    let f = Fixture::with_config(2, [1, 1], 0, |slot, vm| {
        let mut owners = [0; pauli_config::MAX_GUEST_VCPUS as usize];
        owners[0] = (1 - slot) as u8;
        vm.cpu_affinity = pauli_config::CpuAffinity::Physical(owners);
        *vm = vm.renamed(["manager", "service"][slot]).unwrap();
    });
    let rt = &f.runtime;
    rt.board.boot_service.store(true, Relaxed);
    let mut monitor = Monitor::new();
    for slot in 0..2 {
        rt.manager.start(rt.board, slot).unwrap();
        monitor.poll(rt);
    }
    let stats = HsStats::new();
    stats.init_host(2, 0);
    let mut owners = [0, 1].map(|idx| {
        stats.register_owner(idx, idx + 20, false);
        Dispatcher::with_tracer(StatsTracer::with_stats(&stats, idx).unwrap())
    });
    let counts = || {
        [0, 1].map(|i: usize| {
            (
                stats.owners[i].mail_sends.load(Relaxed),
                stats.owners[i].mail_recvs.load(Relaxed),
            )
        })
    };
    let mut run = |slot: usize, op, epoch| {
        let vm = rt.manager.vm(slot).unwrap();
        let cpu = vm.cpu(0).unwrap();
        cpu.data
            .lock()
            .context
            .as_mut()
            .unwrap()
            .script
            .push_back(ExitKind::Control(Control::Management {
                args: [abi::VERSION, op, (2 - slot) as u64, epoch, 0, 0],
            }));
        OWNER.with(|o| o.set(cpu.owner));
        rt.dispatch(&mut owners[cpu.owner]).unwrap();
        assert_eq!(cpu.data.lock().state, CpuState::Blocked);
        let before = counts();
        OWNER.with(|o| o.set(0));
        monitor.poll(rt);
        assert_eq!(
            counts(),
            before,
            "the management pump must not update another owner's slot"
        );
        OWNER.with(|o| o.set(cpu.owner));
        rt.dispatch(&mut owners[cpu.owner]).unwrap();
        cpu.data.lock().context.as_ref().unwrap().reply.unwrap()
    };
    let reply = |error, value| ControlReply::Transport { error, value };
    assert_eq!(run(1, abi::MAIL_RECV, 0), reply(0, 0));
    assert_eq!(run(0, abi::MAIL_SEND, 0), reply(0, 0)); // Present zero-byte message.
    assert_eq!(run(1, abi::MAIL_RECV, 0), reply(0, 1 << 51));
    assert_eq!(run(0, abi::MAIL_SEND, 99), reply(abi::INVALID_STATE, 0));
    assert_eq!(run(1, abi::MAIL_RECV, 0), reply(0, 0));
    assert_eq!(counts(), [(0, 1), (1, 0)]);
}

fn wire(
    f: &Fixture,
    monitor: &mut Monitor,
    source: usize,
    op: u64,
    meta: u64,
    payload: u64,
) -> abi::Return {
    let rt = &f.runtime;
    let epoch = rt.manager.vm(source).unwrap().epoch();
    let peer = if source == 0 { 2 } else { 1 };
    let ticket = rpc::submit(
        rt,
        source,
        0,
        epoch,
        [abi::VERSION, op, peer, meta, payload, 0],
    )
    .unwrap_or_else(|error| panic!("submit opcode={op} metadata={meta:#x}: {error:?}"));
    monitor.poll(rt);
    let Poll::Complete(reply) = rpc::poll(rt, ticket) else {
        panic!("no management reply")
    };
    reply
}

#[test]
fn boot_service_mail_works_in_both_start_orders_without_management_rights() {
    for first in [0, 1] {
        let f = Fixture::with_config(1, [1, 1], 0, |slot, vm| {
            *vm = vm.renamed(["manager", "service"][slot]).unwrap();
        });
        let rt = &f.runtime;
        rt.board
            .boot_service
            .store(true, core::sync::atomic::Ordering::Relaxed);
        let mut monitor = Monitor::new();
        rt.manager.start(rt.board, first).unwrap();
        monitor.poll(rt);
        rt.manager.start(rt.board, 1 - first).unwrap();
        monitor.poll(rt);
        let manager = rt.manager.vm(0).unwrap();
        let service = rt.manager.vm(1).unwrap();
        let ok = abi::Return { error: 0, value: 0 };
        let denied = abi::Return::error(abi::DENIED);
        let stale = abi::Return::error(abi::INVALID_STATE);
        // Send six binary bytes, then an empty tagged message. Source identity
        // and epoch come from the submitting vCPU, never the request payload.
        let data = 0x00fe_341e_ff00;
        let meta = (6 << 32) | (199 << 40);
        assert_eq!(wire(&f, &mut monitor, 0, abi::MAIL_SEND, meta, data), ok);
        assert_eq!(wire(&f, &mut monitor, 0, abi::MAIL_SEND, 255 << 40, 0), ok);
        assert_eq!(
            wire(&f, &mut monitor, 1, abi::MAIL_RECV, 0, 0),
            abi::Return {
                error: 0,
                value: data | (6 << 48) | (1 << 51) | (1 << 52) | (199 << 53)
            }
        );
        assert_eq!(
            wire(&f, &mut monitor, 1, abi::MAIL_RECV, 0, 0),
            abi::Return {
                error: 0,
                value: (1 << 51) | (255 << 53)
            }
        );
        assert_eq!(wire(&f, &mut monitor, 1, abi::MAIL_RECV, 0, 0), ok);
        assert_eq!(wire(&f, &mut monitor, 1, abi::MAIL_SEND, 0, 0), ok);
        assert_eq!(
            wire(
                &f,
                &mut monitor,
                0,
                abi::MAIL_RECV,
                service.epoch().into(),
                0
            ),
            abi::Return {
                error: 0,
                value: 1 << 51
            }
        );
        let epoch = u64::from(manager.epoch());
        for (op, payload) in [
            (0, 0),
            (1, 0),
            (2, 0),
            (3, u64::from(b'x')),
            (4, 0),
            (abi::MEMORY_BYTES, 0),
            (abi::CONSOLE_READ, 0),
        ] {
            assert_eq!(
                wire(
                    &f,
                    &mut monitor,
                    1,
                    op,
                    if op == 0 { 0 } else { epoch },
                    payload
                ),
                denied,
                "opcode {op}"
            );
        }
        // The reverse direction is not re-granted on ordinary polls.
        rt.board.input.lock().extend(b"vm revoke service manager\n");
        monitor.poll(rt);
        assert_eq!(wire(&f, &mut monitor, 1, abi::MAIL_SEND, 0, 0), denied);
        assert_eq!(wire(&f, &mut monitor, 1, abi::MAIL_RECV, 0, 0), denied);
        assert_eq!(wire(&f, &mut monitor, 0, abi::MAIL_SEND, 0, 0), ok);
        let old_service = service.epoch();
        service.host_stop();
        let mut owner = Dispatcher::new();
        for _ in 0..2 {
            rt.dispatch(&mut owner).unwrap();
        }
        monitor.poll(rt);
        assert!(service.lifecycle.joined());
        assert_eq!(wire(&f, &mut monitor, 0, abi::MAIL_SEND, 0, 0), stale);
        rt.manager.start(rt.board, 1).unwrap();
        monitor.poll(rt);
        assert_ne!(service.epoch(), old_service);
        // Its own Ready edge binds MAIL back to its manager again; the manager's
        // old forward grant remains bound to the retired service epoch.
        assert_eq!(wire(&f, &mut monitor, 1, abi::MAIL_RECV, 0, 0), ok);
        assert_eq!(wire(&f, &mut monitor, 0, abi::MAIL_RECV, 0, 0), stale);
        assert_eq!(
            wire(&f, &mut monitor, 0, abi::MAIL_RECV, old_service.into(), 0),
            stale
        );
        assert_eq!(
            wire(
                &f,
                &mut monitor,
                0,
                abi::MAIL_RECV,
                service.epoch().into(),
                0
            ),
            denied
        );
        let old_manager = manager.epoch();
        manager.host_stop();
        for _ in 0..2 {
            rt.dispatch(&mut owner).unwrap();
        }
        monitor.poll(rt);
        rt.manager.start(rt.board, 0).unwrap();
        monitor.poll(rt);
        assert_ne!(manager.epoch(), old_manager);
        assert_eq!(
            wire(&f, &mut monitor, 1, abi::MAIL_SEND, old_manager.into(), 0),
            stale
        );
        assert_eq!(wire(&f, &mut monitor, 1, abi::MAIL_SEND, 0, 0), ok);
        assert_eq!(
            wire(&f, &mut monitor, 0, abi::MAIL_RECV, 0, 0),
            abi::Return {
                error: 0,
                value: 1 << 51
            }
        );
    }
}

fn byte(n: u8) -> MailMessage {
    MailMessage {
        tag: n,
        len: 1,
        data: [n, 0, 0, 0, 0, 0],
    }
}

#[test]
fn full_inbox_keeps_terminal_messages_but_recovers_capacity_for_new_incarnations() {
    for destroy in [false, true] {
        let f = Fixture::with_pool(1, [1, 1], 66);
        let rt = &f.runtime;
        let mut backend = Target(rt);
        let (source, receiver) = (VmId(1), VmId(2));
        backend.start(source).unwrap();
        backend.start(receiver).unwrap();
        let source_epoch = backend.info(source).unwrap().epoch;
        let receiver_epoch = backend.info(receiver).unwrap().epoch;
        let name = ChildName::new("child").unwrap().into();
        let original = backend.create(None, source, name).unwrap();
        let child = original.vm;
        backend.start(child).unwrap();
        for n in 0..8 {
            backend
                .mail_send(child, original.epoch, receiver, receiver_epoch, byte(n))
                .unwrap();
            backend
                .mail_send(source, source_epoch, receiver, receiver_epoch, byte(32 + n))
                .unwrap();
        }
        assert_eq!(
            backend.mail_send(receiver, receiver_epoch, receiver, receiver_epoch, byte(16)),
            Err(Error::Busy)
        );
        let slot = (child.0 - 1) as usize;
        rt.manager.vm(slot).unwrap().request_stop(destroy, false);
        assert_eq!(
            backend.mail_send(receiver, receiver_epoch, child, original.epoch, byte(0)),
            Err(Error::NotActive)
        );
        let mut owner = Dispatcher::new();
        for _ in 0..3 {
            rt.dispatch(&mut owner).unwrap();
        }
        rt.manager.service(rt.board);
        let child_vm = rt.manager.vm(slot).unwrap();
        assert_eq!(
            child_vm.lifecycle.state(),
            if destroy {
                State::Faulted
            } else {
                State::Stopped
            }
        );
        assert_eq!(
            backend.mail_send(receiver, receiver_epoch, child, original.epoch, byte(0)),
            Err(Error::NotActive)
        );
        assert_eq!(
            backend.mail_send(receiver, receiver_epoch, receiver, receiver_epoch, byte(16)),
            Err(Error::Busy)
        );
        assert_eq!(
            backend.mail_recv(receiver, receiver_epoch, child, original.epoch),
            Ok(MailChunk {
                message: Some(byte(0)),
                more: true
            })
        );
        backend
            .mail_send(receiver, receiver_epoch, receiver, receiver_epoch, byte(16))
            .unwrap();
        drop(child_vm);
        if destroy {
            backend.destroy(child, original.epoch).unwrap();
            assert_eq!(backend.create(None, source, name).unwrap().vm, child);
        }
        backend.start(child).unwrap();
        let fresh = backend.info(child).unwrap().epoch;
        assert_ne!(fresh, original.epoch);
        assert_eq!(
            backend.mail_recv(receiver, receiver_epoch, child, original.epoch),
            Err(Error::StaleEpoch)
        );
        for n in 0..7 {
            backend
                .mail_send(child, fresh, receiver, receiver_epoch, byte(64 + n))
                .unwrap();
        }
        assert_eq!(
            backend.mail_send(child, fresh, receiver, receiver_epoch, byte(71)),
            Err(Error::Busy)
        );
        assert_eq!(
            backend.mail_recv(receiver, receiver_epoch, receiver, receiver_epoch),
            Ok(MailChunk {
                message: Some(byte(16)),
                more: false
            })
        );
        backend
            .mail_send(child, fresh, receiver, receiver_epoch, byte(71))
            .unwrap();
        for n in 0..8 {
            assert_eq!(
                backend.mail_recv(receiver, receiver_epoch, source, source_epoch),
                Ok(MailChunk {
                    message: Some(byte(32 + n)),
                    more: n < 7
                })
            );
        }
        // Free global slots do not change the cap of eight for a single peer.
        assert_eq!(
            backend.mail_send(child, fresh, receiver, receiver_epoch, byte(72)),
            Err(Error::Busy)
        );
        for n in 0..8 {
            assert_eq!(
                backend.mail_recv(receiver, receiver_epoch, child, fresh),
                Ok(MailChunk {
                    message: Some(byte(64 + n)),
                    more: n < 7
                })
            );
        }
        backend
            .mail_send(child, fresh, receiver, receiver_epoch, byte(88))
            .unwrap();
        backend.stop(receiver).unwrap();
        for _ in 0..3 {
            rt.dispatch(&mut owner).unwrap();
        }
        rt.manager.service(rt.board);
        backend.start(receiver).unwrap();
        let replacement = backend.info(receiver).unwrap().epoch;
        assert_ne!(replacement, receiver_epoch);
        assert_eq!(
            backend.mail_recv(receiver, receiver_epoch, child, fresh),
            Err(Error::StaleEpoch)
        );
        assert_eq!(
            backend.mail_send(child, fresh, receiver, receiver_epoch, byte(1)),
            Err(Error::StaleEpoch)
        );
        assert_eq!(
            backend.mail_recv(receiver, replacement, child, fresh),
            Ok(MailChunk {
                message: None,
                more: false
            })
        );
    }
}
