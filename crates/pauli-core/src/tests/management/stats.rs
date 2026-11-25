//! A real monitor grants only the selected boot epoch, before servicing its queue.
use super::Fixture;
use crate::{
    board::BootImages,
    manager::VmStore,
    monitor::{Monitor, rpc},
    runtime::{Dispatcher, VmRuntime},
    tests::model::{Arch, TestBoard},
    trace::HS_STATS_MAGIC,
};
use pauli_control::{abi, queue::Poll};
use std::boxed::Box;

fn read<const STATS: bool>(
    rt: &VmRuntime<Arch, TestBoard<STATS>>,
    monitor: &mut Monitor,
    slot: usize,
    selector: u64,
) -> abi::Return {
    let source = rt.manager.vm(slot).unwrap().epoch();
    let args = [abi::VERSION, abi::STATS_READ, 0, selector, 0, 0];
    let ticket = rpc::submit(rt, slot, 0, source, args).unwrap();
    monitor.poll(rt);
    let Poll::Complete(result) = rpc::poll(rt, ticket) else {
        panic!("missing STATS_READ reply")
    };
    result
}

#[test]
fn host_stats_binds_only_the_ready_manager_epoch_before_collector_checks() {
    for grant_manager in [false, true] {
        let f = Fixture::with_config(1, [1; 2], 0, |slot, vm| {
            *vm = vm.renamed(["manager", "service"][slot]).unwrap();
            vm.host_stats = slot == 0 && grant_manager;
        });
        let rt = &f.runtime;
        rt.manager.start(rt.board, 0).unwrap();
        rt.manager.start(rt.board, 1).unwrap();
        let mut monitor = Monitor::new();
        let expected = if grant_manager {
            abi::NOT_SUPPORTED
        } else {
            abi::DENIED
        };
        // Header and a syntactically valid redacted word above the host's owner count.
        for selector in [0, 1 | (63 << 8) | (10 << 16)] {
            assert_eq!(
                read(rt, &mut monitor, 0, selector),
                abi::Return::error(expected)
            );
            assert_eq!(
                read(rt, &mut monitor, 1, selector),
                abi::Return::error(abi::DENIED)
            );
        }
        let manager = rt.manager.vm(0).unwrap();
        let old = manager.epoch();
        let stale =
            rpc::submit(rt, 0, 0, old, [abi::VERSION, abi::STATS_READ, 0, 0, 0, 0]).unwrap();
        manager.host_stop();
        let mut owner = Dispatcher::new();
        for _ in 0..2 {
            rt.dispatch(&mut owner).unwrap();
        }
        rt.manager.service(rt.board);
        rt.manager.start(rt.board, 0).unwrap();
        assert_ne!(manager.epoch(), old);
        monitor.poll(rt);
        assert_eq!(
            rpc::poll(rt, stale),
            Poll::Complete(abi::Return::error(abi::DENIED))
        );
        assert_eq!(read(rt, &mut monitor, 0, 0), abi::Return::error(expected));
    }
}

#[test]
fn enabled_board_exposes_v2_to_manager_and_checks_native_owner_bounds() {
    let (board, _backing, _pool) =
        Fixture::test_board::<true>(1, [1; 2], 0, [None; pauli_config::MAX_DISKS], |slot, vm| {
            vm.host_stats = slot == 0;
            vm.autostart = true;
            BootImages {
                kernel: &[0x31; 64],
                initrd: &[],
                dtb: &[0xd1; 64],
            }
        });
    let rt = Box::leak(Box::new(
        VmRuntime::<Arch, _>::create(
            Box::leak(Box::new(board)),
            Box::leak(Box::new(VmStore::new())),
        )
        .unwrap(),
    ));
    rt.start_workers().unwrap();
    let mut monitor = Monitor::new();
    let success = |value| abi::Return { error: 0, value };
    assert_eq!(read(rt, &mut monitor, 0, 0), success(HS_STATS_MAGIC));
    assert_eq!(read(rt, &mut monitor, 0, 4 << 16), success(1));
    assert_eq!(read(rt, &mut monitor, 0, 1), success(0x40));
    assert_eq!(read(rt, &mut monitor, 0, 1 | (11 << 16)), success(0));
    assert_eq!(
        read(rt, &mut monitor, 0, 1 | (1 << 8) | (11 << 16)),
        abi::Return::error(abi::INVALID_PARAM)
    );
    assert_eq!(
        read(rt, &mut monitor, 1, 0),
        abi::Return::error(abi::DENIED)
    );
}
