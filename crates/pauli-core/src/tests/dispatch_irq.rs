//! Exercise owner IRQ sampling and bank cleanup on the production dispatch path.
//! Ownership/Lock Ordering: each fixture owns its VMs; only the virtual chip is shared.
//! Invariants: wake decisions do not grant entry; every attempted load must be put.
use super::model::{Arch, Fixture, OWNER, Smp};
use crate::{
    arch::{ArchSmp, CpuStatus, ExitKind, Mailbox},
    runtime::Dispatcher,
    vm::{State, Vm, slot::CpuState},
};
mod fence_cancel;
mod kick_overlap;
mod notification;
mod parked;
mod shared_fence;

fn counts(vm: &Vm<Arch>) -> (usize, usize, usize) {
    let chip = vm.devices.irqs.lock();
    (chip.samples.get(), chip.loads, chip.puts)
}

#[test]
fn quick_exits_sample_once_per_entry_even_after_mail() {
    let f = Fixture::new(1, 1);
    let rt = &f.runtime;
    let vm = &rt.manager.vm(0).unwrap();
    rt.manager.start(rt.board, 0).unwrap();
    let mut owner = Dispatcher::new();
    assert!(rt.dispatch(&mut owner).unwrap());
    assert_eq!(counts(vm), (3, 3, 1)); // Three quick exits in one absolute budget.
    let slot = vm.cpu(0).unwrap();
    slot.mailbox.lock().post_ipi();
    Smp::kick(slot.owner).unwrap();
    assert!(rt.dispatch(&mut owner).unwrap());
    assert_eq!(counts(vm), (6, 6, 2)); // The mailbox-only pass used no resident bank.
    assert_eq!(slot.data.lock().context.as_ref().unwrap().ipis, 1);
}

#[test]
fn rejected_native_kick_reports_failure_without_retracting_the_guest_ipi() {
    use crate::arch::{Control, ControlError, ControlReply};
    use crate::tests::model::{KICKS, REJECT_KICK_ONCE};

    let f = Fixture::new(2, 2);
    let rt = &f.runtime;
    rt.manager.start(rt.board, 0).unwrap();
    let vm = rt.manager.vm(0).unwrap();
    let target = vm.cpu(0).unwrap();
    let sender = vm.cpu(1).unwrap();
    let entry = (vm.config.guest_base + pauli_config::KERNEL_OFFSET) as usize;
    sender.request_start(entry, 0).unwrap();
    sender
        .data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Control(Control::Ipi { mask: 1, base: 0 }));

    OWNER.with(|o| o.set(sender.owner));
    REJECT_KICK_ONCE.with(|o| o.set(Some(target.owner)));
    KICKS.with(|n| n.set(0));
    assert!(rt.dispatch(&mut Dispatcher::new()).unwrap());
    assert_eq!(
        sender.data.lock().context.as_ref().unwrap().reply,
        Some(ControlReply::Error(ControlError::Failed))
    );
    assert_eq!(KICKS.with(|n| n.get()), 1);
    assert!(target.mailbox.lock().pending());

    // An independent dispatch can still deliver the published IPI, once.
    OWNER.with(|o| o.set(target.owner));
    let mut local = Dispatcher::new();
    for _ in 0..2 {
        assert!(rt.dispatch(&mut local).unwrap());
        assert_eq!(target.data.lock().context.as_ref().unwrap().ipis, 1);
    }
    assert!(!target.mailbox.lock().pending());
}

#[test]
fn stopped_and_sleeping_vcpus_do_not_borrow_a_bank() {
    let f = Fixture::new(2, 2);
    let rt = &f.runtime;
    let lanes = |owner| [0, 1].map(|vm| rt.manager.placed_on(0, owner).local_cpus(vm));
    assert_eq!(lanes(0), [1, 2]);
    assert_eq!(lanes(1), [2, 1]);
    assert_eq!(lanes(crate::arch::MAX_CPUS), [0, 0]);
    rt.manager.start(rt.board, 0).unwrap();
    let vm = &rt.manager.vm(0).unwrap();
    OWNER.with(|o| o.set(1)); // Secondary vCPU has not been started.
    assert!(!rt.dispatch(&mut Dispatcher::new()).unwrap());
    assert_eq!(counts(vm), (0, 0, 0));
    OWNER.with(|o| o.set(0));
    let mut owner = Dispatcher::new();
    let slot = vm.cpu(0).unwrap();
    slot.data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Wait);
    assert!(rt.dispatch(&mut owner).unwrap());
    assert!(!rt.dispatch(&mut owner).unwrap());
    assert_eq!(
        (slot.status(), slot.run_state()),
        (CpuStatus::Started, CpuState::Waiting)
    );
    assert_eq!(counts(vm), (2, 1, 1)); // WFI still samples the external line.
    vm.devices.irqs.lock().lines[0] = true;
    assert!(rt.dispatch(&mut owner).unwrap());
    assert_eq!(
        (slot.status(), slot.run_state()),
        (CpuStatus::Started, CpuState::Runnable)
    );
    let secondary = vm.cpu(1).unwrap();
    assert_eq!(
        (secondary.status(), secondary.run_state()),
        (CpuStatus::Stopped, CpuState::Stopped)
    );
    assert_eq!(counts(vm), (6, 4, 2)); // One wake decision, three fresh entries.
    assert!(slot.data.lock().context.as_ref().unwrap().external);

    vm.devices.irqs.lock().lines[0] = false;
    slot.data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Wait);
    assert!(rt.dispatch(&mut owner).unwrap());
    slot.mailbox.lock().post_ipi();
    Smp::kick(slot.owner).unwrap();
    assert!(rt.dispatch(&mut owner).unwrap()); // IPI alone still wakes WFI.
    let data = slot.data.lock();
    let cpu = data.context.as_ref().unwrap();
    assert_eq!(
        (data.state, cpu.external, cpu.ipis),
        (CpuState::Runnable, false, 1)
    );
}

#[test]
fn a_wake_sample_is_not_replayed_after_another_vm_runs() {
    let f = Fixture::new(1, 1);
    let rt = &f.runtime;
    for id in [0, 1] {
        rt.manager.start(rt.board, id).unwrap();
    }
    let vm = &rt.manager.vm(0).unwrap();
    let slot = vm.cpu(0).unwrap();
    slot.data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Wait);
    let mut owner = Dispatcher::new();
    assert!(rt.dispatch(&mut owner).unwrap());
    vm.devices.irqs.lock().lines[0] = true;
    // Fence mail selects the WFI owner for maintenance, without injecting an IPI.
    slot.mailbox.lock().post_fence(false, 0);
    Smp::kick(slot.owner).unwrap();
    assert!(rt.dispatch(&mut owner).unwrap()); // Round-robin runs the peer first.
    assert_eq!(slot.data.lock().state, CpuState::Runnable);
    assert_eq!(counts(vm), (2, 1, 1));
    vm.devices.irqs.lock().lines[0] = false;
    assert!(rt.dispatch(&mut owner).unwrap());
    let data = slot.data.lock();
    let cpu = data.context.as_ref().unwrap();
    assert_eq!((cpu.steps, cpu.external, cpu.ipis), (4, false, 0));
}

#[test]
fn partial_load_is_put_before_fault_join_and_cold_restart() {
    let f = Fixture::new(1, 1);
    let rt = &f.runtime;
    for id in [0, 1] {
        rt.manager.start(rt.board, id).unwrap();
    }
    let vm = &rt.manager.vm(0).unwrap();
    let peer = &rt.manager.vm(1).unwrap();
    let peer_epoch = peer.epoch();
    let epoch = vm.epoch();
    vm.devices.irqs.lock().fail_load = true;
    let mut owner = Dispatcher::new();
    assert!(rt.dispatch(&mut owner).unwrap()); // The peer gets its slice despite the failure.
    assert_eq!(counts(vm), (1, 1, 1));
    assert!(vm.try_join(0, rt.manager.link()).unwrap());
    assert_eq!(vm.lifecycle.state(), State::Faulted);
    assert_eq!(
        vm.cpu(0)
            .unwrap()
            .data
            .lock()
            .context
            .as_ref()
            .unwrap()
            .steps,
        0
    );
    assert_eq!(counts(peer), (3, 3, 1));
    rt.manager.start(rt.board, 0).unwrap();
    assert_ne!(vm.epoch(), epoch);
    assert_eq!(counts(vm), (0, 0, 0));
    assert!(rt.dispatch(&mut owner).unwrap());
    assert_eq!(counts(vm), (3, 3, 1));
    assert_eq!(peer.epoch(), peer_epoch);
}

#[test]
fn round_robin_wraps_sparse_slots_and_retires_all_stopping_owners() {
    let f = Fixture::new(1, 2);
    let rt = &f.runtime;
    for id in [0, 1] {
        rt.manager.start(rt.board, id).unwrap();
        let vm = &rt.manager.vm(id).unwrap();
        let entry = (vm.config.guest_base + pauli_config::KERNEL_OFFSET) as usize;
        vm.cpu(1).unwrap().request_start(entry, 0).unwrap();
    }
    let mut owner = Dispatcher::new();
    for turn in 0..8 {
        // Flat slots 0, 1, 4, 5 share one physical CPU, with holes between VMs.
        let vm = &rt.manager.vm((turn / 2) % 2).unwrap();
        let slot = vm.cpu(turn % 2).unwrap();
        let steps = || slot.data.lock().context.as_ref().unwrap().steps;
        let previous = steps();
        assert!(rt.dispatch(&mut owner).unwrap());
        assert_eq!(steps() - previous, 3);
    }
    let vm = &rt.manager.vm(0).unwrap();
    vm.host_stop();
    assert!(rt.dispatch(&mut owner).unwrap()); // Service both retirees, then run the peer.
    assert!(vm.try_join(0, rt.manager.link()).unwrap());
    assert!(vm.retired());
    rt.manager.start(rt.board, 0).unwrap();
    assert!(rt.dispatch(&mut owner).unwrap()); // Peer's second vCPU is next in order.
    assert!(rt.dispatch(&mut owner).unwrap()); // Wrap to the restarted boot vCPU.
    assert_eq!(counts(vm), (3, 3, 1));
}

#[test]
fn a_concurrent_producer_cannot_split_the_entry_snapshot() {
    use std::{sync::Barrier, thread};
    const SLICES: usize = 2048;
    let f = Fixture::new(1, 1);
    let rt = &f.runtime;
    rt.manager.start(rt.board, 0).unwrap();
    let vm = &rt.manager.vm(0).unwrap();
    let chip = &vm.devices.irqs;
    let start = Barrier::new(2);
    thread::scope(|scope| {
        scope.spawn(|| {
            start.wait();
            for i in 0..32_768 {
                chip.lock().lines[0] = i % 2 == 0;
            }
        });
        let mut owner = Dispatcher::new();
        start.wait();
        for _ in 0..SLICES {
            if !rt.dispatch(&mut owner).unwrap() {
                break;
            }
        }
    });
    assert_eq!(chip.lock().split_snapshots, 0);
    assert_eq!(counts(vm), (3 * SLICES, 3 * SLICES, SLICES));
}

/// Development-host timing only: the real dispatcher runs against the modeled ISA.
#[test]
#[ignore = "timing sample; run explicitly in release mode"]
fn sample_quick_exits() {
    sample_shape("sparse", 1, 1);
    sample_shape("dense", 2, 4);
}

fn sample_shape(name: &str, vms: usize, vcpus: u8) {
    use std::time::Instant;
    const SLICES: usize = 65_536;
    let f = Fixture::new(1, vcpus);
    let rt = &f.runtime;
    for id in 0..vms {
        rt.manager.start(rt.board, id).unwrap();
        let vm = &rt.manager.vm(id).unwrap();
        let entry = (vm.config.guest_base + pauli_config::KERNEL_OFFSET) as usize;
        for cpu in 1..usize::from(vcpus) {
            vm.cpu(cpu).unwrap().request_start(entry, 0).unwrap();
        }
    }
    let mut owner = Dispatcher::new();
    let mut samples = [0; 9];
    for round in 0..2 + samples.len() {
        let start = Instant::now();
        for _ in 0..SLICES {
            assert!(rt.dispatch(&mut owner).unwrap());
        }
        let elapsed = start.elapsed().as_nanos();
        if round >= 2 {
            samples[round - 2] = elapsed;
        }
    }
    samples.sort_unstable();
    let entries = (2 + samples.len()) * SLICES * 3;
    let per_cpu = (entries / (vms * usize::from(vcpus))) as u64;
    for vm in rt.manager.vms().take(vms) {
        for slot in vm.cpus() {
            assert_eq!(slot.data.lock().context.as_ref().unwrap().steps, per_cpu);
        }
    }
    std::println!("{name}: {SLICES} slices per batch, 3 entries/slice; batch ns: {samples:?}");
}
