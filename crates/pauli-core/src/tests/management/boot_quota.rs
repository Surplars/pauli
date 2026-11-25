//! Board boot quotas exercised through the resident monitor and scalar guest RPC.
use super::Fixture;
use crate::{
    board::BootImages,
    manager::Budget,
    monitor::{Monitor, rpc},
    runtime::Dispatcher,
    vm::State,
};
use pauli_config::{FirmwareLayout, GuestBoardLayout, MAX_GUEST_DTB, VmConfig};
use pauli_control::{abi, queue::Poll};
use std::{boxed::Box, vec};

const RAM: u64 = 64 << 10;
const QUOTA: Budget = Budget::from_bytes(8, 8, 8 * RAM);

fn template(slot: usize, vm: &mut VmConfig) -> BootImages {
    if slot == 1 {
        vm.memory_size = RAM;
        vm.firmware = Some(FirmwareLayout {
            rx_pages: 1,
            ro_pages: 0,
        });
    }
    let mut dtb = vec![0; MAX_GUEST_DTB];
    let size =
        pauli_config::guest_dtb::riscv(vm, GuestBoardLayout::DEFAULT, 1_000_000, "", &mut dtb)
            .unwrap();
    dtb.truncate(size);
    BootImages {
        kernel: &[0x32; 64],
        initrd: &[],
        dtb: Box::leak(dtb.into_boxed_slice()),
    }
}

fn board(spare_mib: usize, quota: Option<Budget>) -> Fixture {
    let (mut board, banks, pool) = Fixture::test_board(
        1,
        [1; 2],
        spare_mib,
        [None; pauli_config::MAX_DISKS],
        template,
    );
    *board.boot_service.get_mut() = true;
    board.boot_quota = quota;
    Fixture::from_board(board, banks, pool)
}

fn call(f: &Fixture, monitor: &mut Monitor, op: u64, vm: u32, meta: u64, name: u64) -> abi::Return {
    let rt = &f.runtime;
    let source_epoch = rt.manager.vm(0).unwrap().epoch();
    let ticket = rpc::submit(
        rt,
        0,
        0,
        source_epoch,
        [abi::VERSION, op, vm.into(), meta, name, 0],
    )
    .unwrap();
    monitor.poll(rt);
    let Poll::Complete(result) = rpc::poll(rt, ticket) else {
        panic!("RPC incomplete")
    };
    result
}

#[test]
fn boot_service_create_requires_both_quota_and_pool() {
    for (spare, quota) in [(0, None), (2, None), (0, Some(QUOTA)), (2, Some(QUOTA))] {
        let f = board(spare, quota);
        let rt = &f.runtime;
        assert_eq!(rt.manager.budget(0).unwrap(), (Budget::default(), quota));
        assert_eq!(rt.manager.budget(1).unwrap(), (Budget::default(), None));
        rt.manager.start(rt.board, 0).unwrap();
        let mut monitor = Monitor::new();
        let peer = rt.manager.vm(1).unwrap();
        let epoch = u64::from(peer.epoch());
        let name = u64::from(b'x');
        assert_eq!(call(&f, &mut monitor, 0, 2, 0, 0).error, 0);
        assert_eq!(call(&f, &mut monitor, 1, 2, epoch, 0).error, 0);
        assert_eq!(
            call(&f, &mut monitor, abi::CONSOLE_READ, 2, epoch, 0).error,
            0
        );
        assert_eq!(call(&f, &mut monitor, abi::MAIL_RECV, 2, 0, 0).error, 0);
        for (op, field) in [(abi::CREATE_SHAPED, 1 << 32), (abi::CREATE_SHAPED, 1 << 40)] {
            assert_eq!(
                call(&f, &mut monitor, op, 2, epoch | field, name).error,
                abi::DENIED
            );
        }
        for disk in 1..=4 {
            assert_eq!(
                call(
                    &f,
                    &mut monitor,
                    abi::CREATE_DISK,
                    2,
                    epoch | (disk << 56),
                    name
                )
                .error,
                abi::DENIED
            );
        }
        let create = call(&f, &mut monitor, 3, 2, epoch, name);
        if spare > 0 && quota.is_some() {
            assert_eq!((create.error, create.value >> 32), (0, 3));
            assert_eq!(
                rt.manager.budget(0).unwrap(),
                (Budget::from_bytes(1, 1, RAM), quota)
            );
        } else {
            assert_eq!(create, abi::Return::error(abi::DENIED));
            assert_eq!(rt.manager.vms().count(), 2);
        }
    }
}

#[test]
fn eight_firmware_clones_charge_through_stop_and_release_on_destroy() {
    let f = board(2, Some(QUOTA));
    let rt = &f.runtime;
    let manager = &rt.manager;
    manager.start(rt.board, 0).unwrap();
    let peer_epoch = u64::from(manager.vm(1).unwrap().epoch());
    let mut monitor = Monitor::new();
    let mut children: [(u32, u32); 8] = core::array::from_fn(|i| {
        let name = u64::from(b'A' + i as u8);
        let created = call(&f, &mut monitor, 3, 2, peer_epoch, name);
        assert_eq!(created.error, 0, "child {i}");
        let id = (created.value >> 32) as u32;
        let vm = manager.vm(id as usize - 1).unwrap();
        assert_eq!(vm.epoch(), created.value as u32);
        assert_eq!(
            (vm.config.vcpus, vm.config.memory_size, vm.lifecycle.state()),
            (1, RAM, State::Stopped)
        );
        let count = i as u64 + 1;
        assert_eq!(
            manager.budget(0).unwrap(),
            (
                Budget::from_bytes(count as u32, count, count * RAM),
                Some(QUOTA)
            )
        );
        (id, vm.epoch())
    });
    let ninth = u64::from(b'I');
    assert_eq!(
        call(&f, &mut monitor, 3, 2, peer_epoch, ninth),
        abi::Return::error(abi::FAILED)
    );
    assert_eq!(manager.vms().count(), 10);
    // An unsponsored host clone still fits, isolating sponsor exhaustion from a full pool.
    let host = manager
        .provision(
            rt.board,
            None,
            1,
            pauli_control::ChildName::new("host").unwrap().into(),
        )
        .unwrap();
    let host_epoch = manager.vm(host).unwrap().epoch();
    manager.destroy(host, host_epoch).unwrap();

    let (id, epoch) = children[0];
    assert_eq!(call(&f, &mut monitor, 1, id, epoch.into(), 0).error, 0);
    let mut hart = Dispatcher::new();
    for _ in 0..3 {
        rt.dispatch(&mut hart).unwrap();
    }
    assert!(!manager.vm(id as usize - 1).unwrap().retired());
    assert_eq!(
        call(&f, &mut monitor, 4, id, epoch.into(), 0).error,
        abi::INVALID_STATE
    );
    assert_eq!(call(&f, &mut monitor, 2, id, epoch.into(), 0).error, 0);
    rt.dispatch(&mut hart).unwrap();
    monitor.poll(rt);
    assert!(manager.vm(id as usize - 1).unwrap().lifecycle.joined());
    assert_eq!(manager.budget(0).unwrap(), (QUOTA, Some(QUOTA)));
    assert_eq!(call(&f, &mut monitor, 4, id, epoch.into(), 0).error, 0);
    assert_eq!(
        manager.budget(0).unwrap().0,
        Budget::from_bytes(7, 7, 7 * RAM)
    );
    let replacement = call(&f, &mut monitor, 3, 2, peer_epoch, ninth);
    assert_eq!((replacement.error, replacement.value >> 32), (0, id.into()));
    children[0] = (id, replacement.value as u32);
    assert_ne!(children[0].1, epoch);
    for (child, epoch) in children {
        assert_eq!(call(&f, &mut monitor, 4, child, epoch.into(), 0).error, 0);
    }
    assert_eq!(manager.vms().count(), 2);
    assert_eq!(manager.budget(0).unwrap(), (Budget::default(), Some(QUOTA)));
}
