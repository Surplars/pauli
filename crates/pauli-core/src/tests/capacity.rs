//! Resource-derived capacity, including domains above the boot manifest limit.
//! Ownership/Lock Ordering: one simulated hart drives real manager, RPC and join paths.
//! Invariants: oversubscription changes dispatch order, not identity or memory authority.
use super::model::{Fixture, Timer};
use crate::{HvError, arch::ExitKind, runtime::Dispatcher, vm::State};
use pauli_control::ChildName;

fn event(f: &Fixture, slot: usize, kind: ExitKind) {
    let vm = f.runtime.manager.vm(slot).unwrap();
    vm.cpu(0)
        .unwrap()
        .data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(kind);
}
fn slices(f: &Fixture, hart: &mut Dispatcher<Timer>, n: usize) {
    for _ in 0..n {
        assert!(f.runtime.dispatch(hart).unwrap());
    }
}
fn steps(f: &Fixture, slot: usize) -> u64 {
    let vm = f.runtime.manager.vm(slot).unwrap();
    vm.cpu(0)
        .unwrap()
        .data
        .lock()
        .context
        .as_ref()
        .unwrap()
        .steps
}
fn check_two_rounds(f: &Fixture, hart: &mut Dispatcher<Timer>) {
    let rt = &f.runtime;
    assert_eq!(rt.manager.placement_groups(), 2);
    // Wrap only after the second group, even when the first group's low lanes are ready.
    for _ in 0..2 {
        for (id, index) in [(0, 0), (1, 0), (1, 1), (2, 0), (3, 0), (3, 1), (4, 0)] {
            let vm = rt.manager.vm(id).unwrap();
            let cpu = vm.cpu(index).unwrap();
            let before = cpu.data.lock().context.as_ref().unwrap().steps;
            assert!(rt.dispatch(hart).unwrap());
            assert!(cpu.data.lock().context.as_ref().unwrap().steps > before);
        }
    }
}

#[test]
fn fifth_domain_runs_faults_restarts_and_reuses_only_after_join() {
    let f = Fixture::with_pool(1, [1, 2], 198);
    let rt = &f.runtime;
    let create = |template, name| {
        rt.manager.provision(
            rt.board,
            None,
            template,
            ChildName::new(name).unwrap().into(),
        )
    };
    assert_eq!(rt.manager.capacity(), 5); // Two boot + three whole minimum extents.
    for (i, (template, name)) in [(0, "one"), (1, "two"), (0, "three")]
        .into_iter()
        .enumerate()
    {
        assert_eq!(create(template, name).unwrap(), i + 2);
    }
    assert_eq!(create(0, "over"), Err(HvError::Exhausted));
    let mut hart = Dispatcher::new();
    for vm in rt.manager.vms() {
        rt.manager.start(rt.board, vm.slot).unwrap();
        if let Some(cpu) = vm.cpu(1) {
            cpu.request_start(
                vm.config.guest_base as usize + pauli_config::KERNEL_OFFSET as usize,
                0,
            )
            .unwrap();
        }
    }
    check_two_rounds(&f, &mut hart);
    assert!(rt.manager.vms().map(|v| v.config.vcpus).eq([1, 2, 1, 2, 1]));
    for vm in rt.manager.vms() {
        assert!(
            vm.cpus()
                .iter()
                .all(|c| c.data.lock().context.as_ref().unwrap().steps != 0)
        );
    }
    #[cfg(feature = "guest-control")]
    fifth_guest_rpc(&f, &mut hart);
    let peer = rt.manager.vm(1).unwrap().epoch();
    let before = steps(&f, 1);
    let old = rt.manager.vm(4).unwrap().epoch();
    event(&f, 4, ExitKind::Fault);
    slices(&f, &mut hart, 10);
    rt.manager.service(rt.board);
    assert_eq!(rt.manager.vm(4).unwrap().lifecycle.state(), State::Faulted);
    rt.manager.start(rt.board, 4).unwrap();
    let held = rt.manager.vm(4).unwrap();
    let fresh = held.epoch();
    assert_ne!(old, fresh);
    slices(&f, &mut hart, 7);
    held.host_stop();
    assert_eq!(rt.manager.destroy(4, fresh), Err(HvError::Busy));
    slices(&f, &mut hart, 7);
    rt.manager.service(rt.board);
    assert!(held.retired() && held.lifecycle.joined());
    assert_eq!(rt.manager.destroy(4, fresh), Err(HvError::Busy));
    drop(held);
    assert_eq!(rt.manager.destroy(4, old), Err(HvError::Busy));
    rt.manager.destroy(4, fresh).unwrap();
    assert_eq!(rt.manager.placement_groups(), 1);
    assert!(rt.manager.available(4).is_none() && rt.manager.vms().count() == 4);
    assert_eq!(create(0, "reuse").unwrap(), 4);
    assert_eq!(rt.manager.placement_groups(), 2);
    let new = rt.manager.vm(4).unwrap().epoch();
    assert_ne!(new, fresh);
    assert_eq!(rt.manager.destroy(4, fresh), Err(HvError::Busy));
    assert_eq!(rt.manager.vm(1).unwrap().epoch(), peer);
    assert!(steps(&f, 1) > before);
}

#[cfg(feature = "guest-control")]
fn fifth_guest_rpc(f: &Fixture, hart: &mut Dispatcher<Timer>) {
    use crate::{
        arch::{Control, ControlReply},
        monitor::{backend::Target, rpc},
        vm::slot::CpuState,
    };
    use pauli_control::{Caller, Rights, Service, VmId, abi};
    let rt = &f.runtime;
    let vm = rt.manager.vm(4).unwrap();
    let cpu = vm.cpu(0).unwrap();
    let reply = || cpu.data.lock().context.as_ref().unwrap().reply;
    event(f, 4, ExitKind::Control(Control::ManagementProbe));
    slices(f, hart, 7);
    assert_eq!(
        reply(),
        Some(ControlReply::Transport {
            error: 0,
            value: abi::probe(5)
        })
    );
    let peer = rt.manager.vm(1).unwrap();
    let mut service = Service::new();
    service
        .grant(
            Caller::Host,
            vm.epoch(),
            VmId(2),
            Rights::INSPECT,
            &Target(rt),
        )
        .unwrap();
    event(
        f,
        4,
        ExitKind::Control(Control::Management {
            args: [
                abi::VERSION,
                abi::MEMORY_BYTES,
                2,
                peer.epoch() as u64,
                0,
                0,
            ],
        }),
    );
    slices(f, hart, 7);
    assert_eq!(cpu.data.lock().state, CpuState::Blocked);
    rpc::pump(rt, &mut service, true);
    slices(f, hart, 7);
    assert_eq!(
        reply(),
        Some(ControlReply::Transport {
            error: 0,
            value: peer.config.memory_size
        })
    );
}
#[test]
fn a_networkless_domain_never_truncates_its_id_to_a_peer_mac() {
    use crate::backends::net::Net;
    use pauli_config::VmConfig;
    use pauli_devices::net_link::Link;
    use pauli_hal::sync::SpinLock;

    let link = SpinLock::new(Link::new());
    let mut cfg = VmConfig::new("diskless").unwrap();
    for slot in [255, 65_535] {
        assert!(Net::bind(slot, 1, &cfg, &link, None).unwrap().is_none());
    }
    cfg.net_peer = Some(0);
    assert!(matches!(
        Net::bind(255, 1, &cfg, &link, None),
        Err(crate::HvError::InvalidConfig)
    ));
    assert!(Net::bind(1, 1, &cfg, &link, None).unwrap().is_some());
}
