//! Provision heterogeneous guests and inspect the actual DTB at the vCPU boot argument.
//! Ownership/Lock Ordering: host model uses the production manager, RAM boundary and dispatcher.
//! Invariants: guest topology matches admission, retained templates survive cold reboot.
use super::model::{Arch, Fixture};
use crate::{
    HvError,
    board::BootImages,
    monitor::{
        backend::Target,
        commands::{self, Command},
    },
    runtime::Dispatcher,
    vm::Vm,
};
use pauli_config::{GuestBoardLayout, MAX_GUEST_DTB, VmConfig};
use pauli_control::{
    Backend, Caller, ChildName, ChildSpec, Error, Operation, Request, Rights, Service, VmId, abi,
};
use pauli_devices::memory::GuestMemory;
use pauli_hal::fdt::Fdt;
use std::{boxed::Box, format, vec, vec::Vec};

const ARGS: &str = "console=ttyS0 rdinit=/init";
pub(super) fn template(slot: usize, vm: &mut VmConfig) -> BootImages {
    template_with_sstc(slot, vm, false)
}
pub(super) fn template_with_sstc(slot: usize, vm: &mut VmConfig, sstc: bool) -> BootImages {
    vm.memory_size = if slot == 0 { 64 << 20 } else { 96 << 20 };
    vm.aia = slot == 1;
    vm.image.initrd_source = (16 << 20) + 8192;
    vm.image.initrd_size = 4;
    let mut out = vec![0; MAX_GUEST_DTB];
    let n = pauli_config::guest_dtb::riscv_with_sstc(
        vm,
        GuestBoardLayout::DEFAULT,
        1_000_000,
        ARGS,
        sstc,
        &mut out,
    )
    .unwrap();
    out.truncate(n);
    BootImages {
        kernel: &[0x31; 64],
        initrd: b"init",
        dtb: Box::leak(out.into_boxed_slice()),
    }
}
fn spec(command: &str) -> ChildSpec {
    let Command::Create(_, spec) = commands::parse(command).unwrap() else {
        panic!("not create")
    };
    spec
}
pub(super) fn dtb(vm: &Vm<Arch>) -> Vec<u8> {
    let gpa = vm
        .cpu(0)
        .unwrap()
        .data
        .lock()
        .context
        .as_ref()
        .unwrap()
        .boot_opaque as u64;
    assert_eq!(gpa, vm.config.guest_base + vm.config.guest_dtb_offset());
    let mut bytes = vec![0; vm.config.dtb_size as usize];
    assert!(vm.devices.io.lock().ram.read(gpa, &mut bytes));
    bytes
}
fn check_tree(vm: &Vm<Arch>, count: usize, mib: usize, aia: bool) {
    let bytes = dtb(vm);
    let f = Fdt::new(&bytes).unwrap();
    let node = |p: &str| f.find(p).unwrap().unwrap();
    let guest = vm.config.guest_base;
    assert_eq!(vm.cpus().len(), count);
    assert_eq!(vm.devices.io.lock().ram.len(), mib << 20);
    assert_eq!(
        node(&format!("/memory@{guest:x}")).reg().unwrap(),
        Some((guest, (mib as u64) << 20))
    );
    assert_eq!(
        node("/cpus").u32("timebase-frequency").unwrap(),
        Some(1_000_000)
    );
    let layout = GuestBoardLayout::DEFAULT;
    let controller = if aia {
        let ctl = node(&format!("/imsics@{:x}", layout.imsic.base));
        assert_eq!(
            ctl.reg().unwrap(),
            Some((layout.imsic.base, count as u64 * 4096))
        );
        ctl
    } else {
        node(&format!("/plic@{:x}", layout.plic.base))
    };
    let contexts = controller.property("interrupts-extended").unwrap().unwrap();
    assert_eq!(contexts.len(), count * 8);
    for (cpu, pair) in contexts.chunks_exact(8).enumerate() {
        let name = format!("/cpus/cpu@{cpu:x}");
        assert_eq!(node(&name).u32("reg").unwrap(), Some(cpu as u32));
        let phandle = node(&format!("{name}/interrupt-controller"))
            .u32("phandle")
            .unwrap()
            .unwrap();
        assert_eq!(&pair[..4], &phandle.to_be_bytes());
        assert_eq!(&pair[4..], &9u32.to_be_bytes());
    }
    assert!(f.find(&format!("/cpus/cpu@{count:x}")).unwrap().is_none());
    let chosen = node("/chosen");
    assert_eq!(chosen.text("bootargs").unwrap(), Some(ARGS));
    let start = guest + vm.config.initrd_offset();
    for (prop, addr) in [
        ("linux,initrd-start", start),
        ("linux,initrd-end", start + 4),
    ] {
        assert_eq!(
            chosen.property(prop).unwrap(),
            Some(addr.to_be_bytes().as_slice())
        );
    }
    let mut init = [0; 4];
    assert!(vm.devices.io.lock().ram.read(start, &mut init));
    assert_eq!(&init, b"init");
}

#[test]
fn grown_plic_and_shrunken_aia_children_restore_their_own_topologies() {
    let fixture = Fixture::with_templates(1, [1, 4], 164, |slot, vm| {
        template_with_sstc(slot, vm, slot == 1)
    });
    let rt = &fixture.runtime;
    let mut target = Target(rt);
    let grown = target
        .create(
            None,
            VmId(1),
            spec("vm create alpha grow --vcpus 8 --memory-mib 96"),
        )
        .unwrap();
    let small = target
        .create(
            None,
            VmId(2),
            spec("vm create beta small --memory-mib 64 --vcpus 2"),
        )
        .unwrap();
    assert_eq!(
        (
            grown.vcpus,
            grown.memory_bytes,
            small.vcpus,
            small.memory_bytes
        ),
        (8, 96 << 20, 2, 64 << 20)
    );
    let first = rt.manager.vm(2).unwrap();
    let peer = rt.manager.vm(3).unwrap();
    check_tree(&first, 8, 96, false);
    check_tree(&peer, 2, 64, true);
    for vm in [&first, &peer] {
        let bytes = dtb(vm);
        let tree = Fdt::new(&bytes).unwrap();
        for cpu in 0..vm.config.vcpus {
            let node = tree.find(&format!("/cpus/cpu@{cpu:x}")).unwrap().unwrap();
            let isa = node.property("riscv,isa-extensions").unwrap().unwrap();
            assert!(isa.split(|&b| b == 0).any(|ext| ext == b"sstc"));
        }
    }
    check_tree(&rt.manager.vm(0).unwrap(), 1, 64, false);
    check_tree(&rt.manager.vm(1).unwrap(), 4, 96, true);
    target.start(grown.vm).unwrap();
    target.start(small.vm).unwrap();
    let peer_epoch = peer.epoch();
    let old_epoch = first.epoch();
    let mut owner = Dispatcher::new();
    rt.dispatch(&mut owner).unwrap();
    let peer_tree = dtb(&peer);
    let gpa = first.config.guest_base + first.config.guest_dtb_offset();
    assert!(first.devices.io.lock().ram.write(gpa, &[0x55]));
    first.request_stop(true, false);
    rt.dispatch(&mut owner).unwrap();
    rt.manager.service(rt.board);
    target.start(grown.vm).unwrap();
    assert_ne!(first.epoch(), old_epoch);
    check_tree(&first, 8, 96, false);
    assert_eq!((dtb(&peer), peer.epoch()), (peer_tree, peer_epoch));
}

#[test]
fn scalar_creation_requires_explicit_shape_authority_and_a_current_epoch() {
    let fixture = Fixture::with_templates(1, [1, 2], 66, template);
    let rt = &fixture.runtime;
    let mut target = Target(rt);
    let mut svc = Service::new();
    target.start(VmId(1)).unwrap();
    let issuer = target.info(VmId(1)).unwrap().epoch;
    let source = target.info(VmId(2)).unwrap();
    let packed = u64::from(source.epoch) | (16 << 32) | (64 << 40);
    let args = [
        1,
        abi::CREATE_SHAPED,
        2,
        packed,
        u64::from_le_bytes(*b"worker\0\0"),
        0,
    ];
    let req = abi::decode(args).unwrap();
    for right in [Rights::ALL, Rights::CREATE, Rights::SHAPE] {
        svc.grant(Caller::Host, issuer, source.vm, right, &target)
            .unwrap();
        assert_eq!(
            svc.execute(Caller::Guest(issuer), req, &mut target),
            Err(Error::Denied)
        );
    }
    svc.grant(
        Caller::Host,
        issuer,
        source.vm,
        Rights::CREATE.union(Rights::SHAPE),
        &target,
    )
    .unwrap();
    assert_eq!(
        svc.execute(
            Caller::Guest(issuer),
            Request { epoch: 0, ..req },
            &mut target
        ),
        Err(Error::StaleEpoch)
    );
    assert_eq!(rt.manager.find("worker"), Err(HvError::NotFound));
    let child = svc
        .execute(Caller::Guest(issuer), req, &mut target)
        .unwrap()
        .info;
    assert_eq!((child.vcpus, child.memory_bytes), (16, 64 << 20));
    let stale = Request {
        vm: child.vm,
        epoch: 0,
        operation: Operation::MemoryBytes,
    };
    assert_eq!(
        svc.execute(Caller::Guest(issuer), stale, &mut target),
        Err(Error::StaleEpoch)
    );
    check_tree(&rt.manager.vm(2).unwrap(), 16, 64, true);
    svc.execute(
        Caller::Guest(issuer),
        Request {
            vm: child.vm,
            epoch: child.epoch,
            operation: Operation::Destroy,
        },
        &mut target,
    )
    .unwrap();
}

#[test]
fn opaque_trees_and_unauthorized_pin_growth_do_not_consume_the_spare_extent() {
    let fixture = Fixture::with_templates(2, [1, 2], 98, |slot, vm| {
        let mut images = template(slot, vm);
        if slot == 1 {
            let mut pins = [0; pauli_config::MAX_GUEST_VCPUS as usize];
            pins[..2].fill(1);
            vm.cpu_affinity = pauli_config::CpuAffinity::Physical(pins);
        } else {
            // A valid but custom model string: rebuilding it would lose platform policy.
            let mut bytes = images.dtb.to_vec();
            let model = b"OpenIon isolated";
            let i = bytes.windows(model.len()).position(|s| s == model).unwrap();
            bytes[i] = b'X';
            Fdt::new(&bytes).unwrap();
            images.dtb = Box::leak(bytes.into_boxed_slice());
        }
        images
    });
    let rt = &fixture.runtime;
    let mut target = Target(rt);
    assert_eq!(
        target.create(None, VmId(1), spec("vm create alpha x --vcpus 2")),
        Err(Error::Unsupported)
    );
    assert_eq!(
        target.create(None, VmId(2), spec("vm create beta x --vcpus 3")),
        Err(Error::Unsupported)
    );
    for line in [
        "vm create alpha x --vcpus 17",
        "vm create alpha x --memory-mib 63",
        "vm create alpha x --memory-mib 65",
    ] {
        assert_eq!(
            target.create(None, VmId(1), spec(line)),
            Err(Error::Invalid)
        );
    }
    let child = target
        .create(None, VmId(1), ChildName::new("x").unwrap().into())
        .unwrap();
    assert_eq!(child.vm, VmId(3)); // Same opaque tree remains clonable without rewriting.
    target.destroy(child.vm, child.epoch).unwrap();
    let shrunk = target
        .create(None, VmId(2), spec("vm create beta x --vcpus 1"))
        .unwrap();
    let pinned = rt.manager.vm(2).unwrap();
    assert_eq!((shrunk.vcpus, pinned.cpu(0).unwrap().owner), (1, 1));
}
