//! Inspect the immutable Linux DTB at the actual boot argument and every created vCPU.
use super::{
    model::{Arch, Fixture, OWNER, SSTC_OWNERS},
    shape::{dtb, template_with_sstc},
};
use crate::{HvError, board::BootImages, monitor::backend::Target, runtime::Dispatcher, vm::Vm};
use pauli_config::{CpuAffinity, FirmwareLayout, GuestBoardLayout, MAX_GUEST_DTB, MAX_GUEST_VCPUS};
use pauli_control::{Backend, ChildName, ChildSpec, Shape, VmId};
use pauli_devices::memory::GuestMemory;
use pauli_hal::fdt::Fdt;
use std::{boxed::Box, format};

struct OwnerProbes(usize);
impl OwnerProbes {
    fn with(mask: usize) -> Self {
        Self(SSTC_OWNERS.with(|old| old.replace(mask)))
    }
}
impl Drop for OwnerProbes {
    fn drop(&mut self) {
        SSTC_OWNERS.with(|owners| owners.set(self.0));
    }
}

fn check(vm: &Vm<Arch>, enabled: bool) {
    let bytes = dtb(vm);
    let tree = Fdt::new(&bytes).unwrap();
    for slot in vm.cpus() {
        let path = format!("/cpus/cpu@{:x}", slot.index);
        let node = tree.find(&path).unwrap().unwrap();
        let extensions = node.property("riscv,isa-extensions").unwrap().unwrap();
        assert_eq!(extensions.split(|&v| v == 0).any(|s| s == b"sstc"), enabled);
        let legacy = node.text("riscv,isa").unwrap().unwrap();
        assert_eq!(legacy.split('_').any(|s| s == "sstc"), enabled);
        assert_eq!(slot.data.lock().context.as_ref().unwrap().sstc, enabled);
    }
}

#[test]
fn boot_uses_the_supplied_tree_and_every_selected_owner() {
    // The management CPU (0) has no positive probe. Slot 0 is overcommitted
    // onto proven owner 2; slot 1 also uses unproven owner 1.
    let _probes = OwnerProbes::with(1 << 2);
    for advertise_mixed in [false, true] {
        let f = Fixture::with_templates(3, [2, 2], 0, |slot, vm| {
            let mut pins = [0; MAX_GUEST_VCPUS as usize];
            pins[0] = 2;
            pins[1] = if slot == 0 { 2 } else { 1 };
            vm.cpu_affinity = CpuAffinity::Physical(pins);
            template_with_sstc(slot, vm, slot == 0 || advertise_mixed)
        });
        let rt = &f.runtime;
        let admitted = rt.manager.vm(0).unwrap();
        check(&admitted, true);
        if advertise_mixed {
            assert_eq!(rt.manager.vm(1).err(), Some(HvError::Unsupported));
        } else {
            check(&rt.manager.vm(1).unwrap(), false);
        }
        rt.manager.start(rt.board, 0).unwrap();
        let sibling = admitted.cpu(1).unwrap();
        sibling
            .request_start(
                admitted.config.guest_base as usize + pauli_config::KERNEL_OFFSET as usize,
                0,
            )
            .unwrap();
        OWNER.with(|owner| owner.set(2));
        let mut local = Dispatcher::new();
        for _ in 0..3 {
            rt.dispatch(&mut local).unwrap();
        }
        assert!(sibling.data.lock().context.as_ref().unwrap().steps > 0);
        check(&admitted, true);
    }
}

#[test]
fn firmware_remains_on_sbi_even_when_every_owner_supports_direct_compare() {
    let _probes = OwnerProbes::with(0b11);
    let f = Fixture::with_templates(2, [1, 1], 0, |slot, vm| {
        vm.memory_size = 32 * 1024;
        vm.firmware = Some(FirmwareLayout {
            rx_pages: 1,
            ro_pages: 0,
        });
        let mut out = std::vec![0; MAX_GUEST_DTB];
        let size = pauli_config::guest_dtb::riscv_with_sstc(
            vm,
            GuestBoardLayout::DEFAULT,
            1_000_000,
            "",
            true,
            &mut out,
        )
        .unwrap();
        out.truncate(size);
        if slot == 1 {
            // An external producer advertises Sstc in both CPU properties.
            // Keep the firmware memory geometry and all DTB offsets intact.
            for (old, new) in [
                (b"zifencei\0", b"sstc\0zba\0"),
                (b"zifencei_", b"sstc_zba_"),
            ] {
                let pos = out.windows(old.len()).position(|word| word == old).unwrap();
                out[pos..pos + old.len()].copy_from_slice(new);
            }
            assert_eq!(pauli_config::guest_dtb::riscv_sstc(&out, 1), Ok(true));
        }
        BootImages {
            kernel: &[0x31; 64],
            initrd: &[],
            dtb: Box::leak(out.into_boxed_slice()),
        }
    });
    check(&f.runtime.manager.vm(0).unwrap(), false);
    assert_eq!(f.runtime.manager.vm(1).err(), Some(HvError::Unsupported));
}

#[test]
fn clone_uses_its_own_placement_and_keeps_the_admission_across_restart() {
    let _probes = OwnerProbes::with(0b011);
    let f = Fixture::with_templates(3, [2, 1], 100, |slot, vm| {
        template_with_sstc(slot, vm, slot == 0)
    });
    let rt = &f.runtime;
    let mut target = Target(rt);
    let advertised = rt.manager.vm(0).unwrap();
    let base = rt.manager.vm(1).unwrap();
    let parent_tree = dtb(&advertised);
    let base_tree = dtb(&base);
    check(&advertised, true);
    check(&base, false);

    // Same guest shape, different physical owners: [0,1] becomes [2,0].
    let name = ChildName::new("exact").unwrap();
    let exact = target.create(None, VmId(1), name.into()).unwrap();
    let child = rt.manager.vm(exact.vm.0 as usize - 1).unwrap();
    assert_eq!((child.cpus()[0].owner, child.cpus()[1].owner), (2, 0));
    check(&child, false);
    let original = dtb(&child);
    target.start(exact.vm).unwrap();
    let first_epoch = child.epoch();
    let gpa = child.config.guest_base + child.config.guest_dtb_offset();
    assert!(child.devices.io.lock().ram.write(gpa, &[0; 8]));
    target.stop(exact.vm).unwrap();
    rt.manager.service(rt.board);
    // Worker 2's late positive result applies to future VM admission.
    SSTC_OWNERS.with(|owners| owners.set(0b111));
    target.start(exact.vm).unwrap();
    assert_ne!(child.epoch(), first_epoch);
    assert_eq!(dtb(&child), original);
    check(&child, false);
    target.stop(exact.vm).unwrap();
    rt.manager.service(rt.board);
    let epoch = child.epoch();
    drop(child);
    target.destroy(exact.vm, epoch).unwrap();

    let shaped = ChildSpec {
        shape: Shape {
            vcpus: core::num::NonZeroU8::new(3),
            memory_mib: None,
        },
        ..ChildName::new("grown").unwrap().into()
    };
    let grown = target.create(None, VmId(2), shaped).unwrap();
    let child = rt.manager.vm(grown.vm.0 as usize - 1).unwrap();
    assert_eq!(child.config.vcpus, 3);
    check(&child, true);
    assert_eq!(dtb(&base), base_tree);
    assert_eq!(dtb(&advertised), parent_tree);
    check(&base, false);
}

#[test]
fn opaque_trees_remain_byte_exact_and_incompatible_clones_stay_unpublished() {
    let _probes = OwnerProbes::with(1);
    let f = Fixture::with_templates(3, [1, 1], 100, |slot, vm| {
        let mut images = template_with_sstc(slot, vm, slot == 0);
        let mut bytes = images.dtb.to_vec();
        let model = b"OpenIon isolated";
        let pos = bytes.windows(model.len()).position(|b| b == model).unwrap();
        bytes[pos] = b'X';
        images.dtb = Box::leak(bytes.into_boxed_slice());
        images
    });
    let rt = &f.runtime;
    let mut target = Target(rt);
    let name = ChildName::new("opaque").unwrap();
    assert_eq!(
        target.create(None, VmId(1), name.into()),
        Err(pauli_control::Error::Unsupported)
    );
    assert_eq!(rt.manager.find("opaque"), Err(HvError::NotFound));
    SSTC_OWNERS.with(|bits| bits.set(0b111));
    let clone = target.create(None, VmId(2), name.into()).unwrap();
    let vm = rt.manager.vm(clone.vm.0 as usize - 1).unwrap();
    assert_eq!(dtb(&vm), dtb(&rt.manager.vm(1).unwrap()));
    check(&vm, false);
}
