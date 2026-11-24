//! Dynamic volume grants, private discovery/queues and durable handoff through real VM code.
//! Ownership/Lock Ordering: one manager, private fake native disks, no hardware or files.
//! Invariants: denied creation has no effects; a failed or pending join cannot transfer a disk.
use super::{
    model::{Arch, Fixture},
    shape::{dtb, template},
    storage::Native,
};
use crate::{HvError, runtime::Dispatcher, vm::State};
use pauli_config::{DiskConfig, GuestBoardLayout};
use pauli_control::{ChildName, DiskSlot};
use pauli_hal::{
    device::BlockOperation::{Flush, Read, Write},
    fdt::Fdt,
};
mod ring;

fn fixture(spare: usize) -> (Fixture, &'static Native, &'static Native) {
    let (a, b) = (Native::new(), Native::new());
    let disk = |writable| DiskConfig {
        sectors: 64,
        writable,
        managed: true,
    };
    let f = Fixture::with_disks(
        1,
        [1, 1],
        spare,
        [Some((disk(true), a)), Some((disk(false), b)), None, None],
        template,
    );
    (f, a, b)
}
fn create(f: &Fixture, name: &str, disk: Option<u8>) -> crate::Result<usize> {
    let mut spec: pauli_control::ChildSpec = ChildName::new(name).unwrap().into();
    spec.disk = disk.map(|i| DiskSlot::new(i).unwrap());
    f.runtime.manager.provision(f.runtime.board, None, 0, spec)
}
fn advertised(f: &Fixture, slot: usize) -> bool {
    let vm = f.runtime.manager.vm(slot).unwrap();
    let bytes = dtb(&vm);
    let path = std::format!("/virtio@{:x}", GuestBoardLayout::DEFAULT.block.base);
    let found = Fdt::new(&bytes).unwrap().find(&path).unwrap().is_some();
    const DEVICE_ID: usize = 0x08;
    assert_eq!(ring::access(f, &vm, DEVICE_ID, None), found.then_some(2));
    found
}

#[test]
fn high_vm_id_keeps_its_volume_until_write_flush_join_and_destruction() {
    let (f, disk, _) = fixture(198);
    let rt = &f.runtime;
    assert!(!advertised(&f, 0));
    create(&f, "hold", None).unwrap();
    create(&f, "other", Some(1)).unwrap();
    let slot = create(&f, "writer", Some(0)).unwrap();
    assert_eq!(slot, 4); // Fifth VM; not a boot event index.
    assert!(advertised(&f, slot) && advertised(&f, 3));
    rt.manager.start(rt.board, slot).unwrap();
    let mut cpu = Dispatcher::new();
    rt.dispatch(&mut cpu).unwrap();
    let vm = rt.manager.vm(slot).unwrap();
    let epoch = vm.epoch();
    ring::write(&f, &vm, 0x5a);
    assert_eq!(disk.pending(), Some(Write));
    vm.request_stop(true, false);
    drop(vm);
    rt.dispatch(&mut cpu).unwrap();
    for operation in [Write, Flush] {
        rt.manager.service(rt.board);
        assert_eq!(disk.pending(), Some(operation));
        assert_eq!(rt.manager.destroy(slot, epoch), Err(HvError::Busy));
        assert_eq!(create(&f, "contender", Some(0)), Err(HvError::Busy));
        assert_eq!(*disk.durable.lock(), [0; 512]);
        disk.finish(operation, true);
    }
    rt.manager.service(rt.board);
    assert_eq!(*disk.durable.lock(), [0x5a; 512]);
    assert_eq!(
        rt.manager.vm(slot).unwrap().lifecycle.state(),
        State::Faulted
    );
    assert_eq!(create(&f, "contender", Some(0)), Err(HvError::Busy));
    rt.manager.start(rt.board, slot).unwrap();
    let fresh = rt.manager.vm(slot).unwrap().epoch();
    assert_ne!(fresh, epoch);
    assert!(advertised(&f, slot));
    rt.manager.vm(slot).unwrap().host_stop();
    rt.dispatch(&mut cpu).unwrap();
    rt.manager.service(rt.board);
    disk.finish(Flush, true);
    rt.manager.service(rt.board);
    assert_eq!(rt.manager.destroy(slot, epoch), Err(HvError::Busy));
    rt.manager.destroy(slot, fresh).unwrap();
    assert_eq!(create(&f, "next", Some(0)), Ok(slot));
    assert_ne!(rt.manager.vm(slot).unwrap().epoch(), fresh);
    assert_eq!(*disk.durable.lock(), [0x5a; 512]);
    rt.manager.start(rt.board, slot).unwrap();
    let successor = rt.manager.vm(slot).unwrap();
    ring::read(&f, &successor);
    disk.finish(Read, true);
    rt.manager.service(rt.board);
    ring::completed(&successor, 0, 0x5a);
    assert_eq!(rt.manager.volume_owner(1), Some(3));
}

#[test]
fn read_only_child_rejects_a_real_write_before_native_submission() {
    let (f, _, readonly) = fixture(66);
    let slot = create(&f, "reader", Some(1)).unwrap();
    f.runtime.manager.start(f.runtime.board, slot).unwrap();
    let vm = f.runtime.manager.vm(slot).unwrap();
    ring::write(&f, &vm, 0x8c);
    ring::completed(&vm, 1, 0x8c); // VIRTIO_BLK_S_IOERR, with the guest's input untouched.
    assert_eq!(readonly.pending(), None);
    assert_eq!(*readonly.durable.lock(), [0; 512]);
}

#[test]
fn failed_native_mutation_or_final_flush_quarantines_only_its_volume() {
    for fail in [Write, Flush] {
        let (f, disk, _) = fixture(132);
        let rt = &f.runtime;
        let slot = create(&f, "writer", Some(0)).unwrap();
        rt.manager.start(rt.board, slot).unwrap();
        let vm = rt.manager.vm(slot).unwrap();
        let epoch = vm.epoch();
        ring::write(&f, &vm, 0xac);
        vm.host_stop();
        drop(vm);
        rt.dispatch(&mut Dispatcher::new()).unwrap();
        disk.finish(Write, fail != Write);
        rt.manager.service(rt.board);
        if fail == Flush {
            disk.finish(Flush, false);
            rt.manager.service(rt.board);
        }
        for _ in 0..2 {
            rt.manager.service(rt.board);
        }
        assert!(rt.manager.vm(slot).unwrap().join_error().is_some());
        assert_eq!(rt.manager.start(rt.board, slot), Err(HvError::Busy));
        assert_eq!(rt.manager.destroy(slot, epoch), Err(HvError::Busy));
        assert_eq!(create(&f, "blocked", Some(0)), Err(HvError::Busy));
        assert!(create(&f, "peer", Some(1)).is_ok());
        assert_eq!(*disk.durable.lock(), [0; 512]);
    }
}
#[cfg(feature = "guest-control")]
#[test]
fn management_vcpu_can_only_create_with_its_granted_disk_and_template() {
    use crate::{arch::ControlReply, monitor::backend::Target};
    use pauli_control::{Backend, Caller, Rights, Service, VmId, abi};
    let (f, _, _) = fixture(198);
    let mut target = Target(&f.runtime);
    let mut service = Service::new();
    target.start(VmId(1)).unwrap();
    let sender = target.info(VmId(1)).unwrap().epoch;
    let template = target.info(VmId(2)).unwrap();
    let mut cpu = Dispatcher::new();
    let grant = |service: &mut Service, rights: &str| {
        service
            .grant(
                Caller::Host,
                sender,
                template.vm,
                Rights::parse(rights).unwrap(),
                &target,
            )
            .unwrap();
    };
    let word = |disk: u64, epoch: u32| (disk + 1) << 56 | u64::from(epoch);
    let name = |c: u8| u64::from_le_bytes([c, 0, 0, 0, 0, 0, 0, 0]);
    let call = |s: &mut Service, cpu: &mut _, packed, label| {
        let req = [1, abi::CREATE_DISK, 2, packed, name(label), 0];
        super::provision::guest_call(&f, s, cpu, req)
    };
    let reject = |error| ControlReply::Transport { error, value: 0 };
    let requested = word(0, template.epoch);
    for right in ["all", "create", "create,shape", "create,disk1", "disk0"] {
        grant(&mut service, right);
        assert_eq!(
            call(&mut service, &mut cpu, requested, b'a'),
            reject(abi::DENIED)
        );
    }
    grant(&mut service, "create,disk0,disk1");
    assert_eq!(
        call(&mut service, &mut cpu, requested | (2 << 32), b'a'),
        reject(abi::DENIED)
    );
    assert_eq!(
        call(&mut service, &mut cpu, word(0, sender), b'a'),
        reject(abi::INVALID_STATE)
    );
    for (disk, label, id) in [(0, b'w', 3u32), (1, b'r', 4)] {
        let result = call(&mut service, &mut cpu, word(disk, template.epoch), label);
        let child = target.info(VmId(id)).unwrap();
        assert_eq!(
            result,
            ControlReply::Transport {
                error: 0,
                value: (u64::from(id) << 32) | u64::from(child.epoch)
            }
        );
        let vm = f.runtime.manager.vm(id as usize - 1).unwrap();
        assert_eq!(
            (vm.config.disk, vm.config.writable),
            (Some(disk as u8), disk == 0)
        );
        const FEATURES: usize = 0x10;
        const READ_ONLY: usize = 1 << 5;
        assert_eq!(
            ring::access(&f, &vm, FEATURES, None).unwrap() & READ_ONLY != 0,
            disk == 1
        );
    }
    assert_eq!(
        call(&mut service, &mut cpu, requested, b'x'),
        reject(abi::INVALID_STATE)
    );
    service.revoke(Caller::Host, sender, template.vm).unwrap();
    assert_eq!(
        call(&mut service, &mut cpu, requested, b'y'),
        reject(abi::DENIED)
    );
    assert!(f.runtime.manager.find("x").is_err() && f.runtime.manager.find("y").is_err());
}

#[test]
fn boot_device_or_unpublished_build_never_becomes_an_implicit_lease() {
    let disk = Native::new();
    let boot = DiskConfig {
        sectors: 64,
        writable: true,
        managed: false,
    };
    let f = Fixture::with_disks(
        1,
        [1, 1],
        66,
        [Some((boot, disk)), None, None, None],
        |i, vm| {
            if i == 1 {
                vm.disk = Some(0);
                vm.writable = true;
            }
            template(i, vm)
        },
    );
    assert_eq!(create(&f, "notmine", Some(0)), Err(HvError::Permission));
    assert_eq!(create(&f, "absent", Some(2)), Err(HvError::NotFound));
    let (f, _, _) = fixture(66);
    super::model::REJECT_NEXT_SPACE.with(|v| v.set(true));
    assert_eq!(create(&f, "fail", Some(0)), Err(HvError::Fault));
    assert_eq!(f.runtime.manager.volume_owner(0), None);
    assert!(create(&f, "retry", Some(0)).is_ok());
}
