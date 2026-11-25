//! Exact output bounds and fail-closed Linux guest geometry.
//! Ownership/Lock Ordering: each test owns its VM config and guarded byte buffer.
//! Invariants: no out-of-bounds output, overlapping mappings or wrapped offsets.
use crate::{
    Error, FirmwareLayout, GuestBoardLayout, MAX_GUEST_DTB, MAX_GUEST_VCPUS, VmConfig,
    guest_dtb::{riscv, riscv_configured, riscv_sstc, riscv_with_sstc, validate_control_events},
};

#[test]
fn control_node_follows_the_explicit_option_and_the_selected_controller() {
    use pauli_hal::fdt::Fdt;
    let mut vm = VmConfig::new("peer").unwrap();
    let mut layout = GuestBoardLayout::DEFAULT;
    layout.control_irq = 17;
    let (mut old, mut buf) = ([0; MAX_GUEST_DTB], [0; MAX_GUEST_DTB]);
    for aia in [false, true] {
        vm.aia = aia;
        for sstc in [false, true] {
            vm.control_events = false;
            let old_len = riscv_with_sstc(&vm, layout, 1, "", sstc, &mut old).unwrap();
            let n = riscv_configured(&vm, layout, 1, "", sstc, &mut buf).unwrap();
            assert_eq!(&buf[..n], &old[..old_len]);
            vm.control_events = true;
            let n = riscv_with_sstc(&vm, layout, 1, "", sstc, &mut buf).unwrap();
            assert_eq!(&buf[..n], &old[..old_len]); // Legacy writer always suppresses this opt-in.
            assert_eq!(
                validate_control_events(&buf[..n], &vm, layout),
                Err(Error::Dtb)
            );
            let n = riscv_configured(&vm, layout, 1, "", sstc, &mut buf).unwrap();
            let bytes = &buf[..n];
            assert_eq!(riscv_sstc(bytes, 1), Ok(sstc));
            assert_eq!(validate_control_events(bytes, &vm, layout), Ok(()));
            let node = Fdt::new(bytes)
                .unwrap()
                .find("/pauli-control")
                .unwrap()
                .unwrap();
            assert_eq!(node.reg().unwrap(), None);
            assert_eq!(
                node.text("compatible").unwrap(),
                Some("openion,pauli-control")
            );
            assert_eq!(node.u32("interrupt-parent").unwrap(), Some(1));
            let spec = node.property("interrupts").unwrap().unwrap();
            let expected = [0, 0, 0, 17, 0, 0, 0, 4];
            assert_eq!(spec, &expected[..if aia { 8 } else { 4 }]);
            vm.control_events = false;
            assert_eq!(validate_control_events(bytes, &vm, layout), Err(Error::Dtb));
        }
    }
}

#[test]
fn opaque_control_tree_requires_the_configured_source_and_an_actual_matching_parent() {
    use pauli_hal::fdt::Fdt;
    let mut vm = VmConfig::new("opaque").unwrap();
    vm.control_events = true;
    vm.aia = true;
    let layout = GuestBoardLayout::DEFAULT;
    let mut buf = [0; MAX_GUEST_DTB];
    let n = riscv_configured(&vm, layout, 1, "", false, &mut buf).unwrap();
    let model = buf[..n].windows(7).position(|v| v == b"OpenIon").unwrap();
    buf[model] = b'X';
    assert_eq!(validate_control_events(&buf[..n], &vm, layout), Ok(()));
    for (path, property, byte) in [
        ("/pauli-control", "interrupt-parent", 3),
        ("/pauli-control", "interrupts", 3),
        ("/pauli-control", "interrupts", 7),
        ("/aplic@c000000", "phandle", 3),
        ("/aplic@c000000", "reg", 7),
        ("/aplic@c000000", "#interrupt-cells", 3),
        ("/cpus/cpu@0/interrupt-controller", "phandle", 3),
    ] {
        let node = Fdt::new(&buf[..n]).unwrap().find(path).unwrap().unwrap();
        let pos = node.property(property).unwrap().unwrap().as_ptr() as usize
            - buf.as_ptr() as usize
            + byte;
        let original = buf[pos];
        buf[pos] = if path.starts_with("/cpus/") {
            1
        } else {
            original ^ 1
        };
        assert_eq!(
            validate_control_events(&buf[..n], &vm, layout),
            Err(Error::Dtb),
            "{path} {property}"
        );
        buf[pos] = original;
    }
}

#[test]
fn exact_sized_buffers_match_staging_and_short_buffers_never_touch_neighbors() {
    let mut vm = VmConfig::new("shape").unwrap();
    let layout = GuestBoardLayout::DEFAULT;
    for aia in [false, true] {
        vm.aia = aia;
        for n in 1..=MAX_GUEST_VCPUS {
            vm.vcpus = n;
            let mut expected = vec![0x99; MAX_GUEST_DTB];
            let len = riscv(&vm, layout, 37_000_000, "console=ttyS0", &mut expected).unwrap();
            for limit in [0, 39, 55, 56, 64, len - 1, len] {
                let mut guarded = vec![0xab; limit + 2];
                let result = riscv(
                    &vm,
                    layout,
                    37_000_000,
                    "console=ttyS0",
                    &mut guarded[1..=limit],
                );
                if limit == len {
                    assert_eq!(result, Ok(len));
                    assert_eq!(&guarded[1..=limit], &expected[..len]);
                } else {
                    assert_eq!(result, Err(Error::Dtb));
                }
                assert_eq!((guarded[0], guarded[limit + 1]), (0xab, 0xab));
            }
        }
    }
}

#[test]
fn reject_unrepresentable_guest_memory_before_emitting_a_tree() {
    let mut vm = VmConfig::new("shape").unwrap();
    let mut layout = GuestBoardLayout::DEFAULT;
    let mut buf = [0xa3; 64];
    layout.uart.base = vm.guest_base + vm.memory_size - 1;
    assert_eq!(riscv(&vm, layout, 1, "", &mut buf), Err(Error::Memory));
    assert_eq!(buf, [0xa3; 64]);
    layout = GuestBoardLayout::DEFAULT;
    vm.guest_base = u64::MAX & !(crate::ALIGN - 1);
    assert_eq!(riscv(&vm, layout, 1, "", &mut buf), Err(Error::Memory));
    vm.guest_base = layout.ram_base;
    vm.image.kernel_memory_size = u64::MAX;
    assert_eq!(riscv(&vm, layout, 1, "", &mut buf), Err(Error::Image));
    vm.image.kernel_memory_size = 0;
    vm.image.initrd_size = vm.memory_size;
    assert_eq!(riscv(&vm, layout, 1, "", &mut buf), Err(Error::Image));
    vm.image.initrd_size = 0;
    for n in [0, MAX_GUEST_VCPUS + 1, 255] {
        vm.vcpus = n;
        assert_eq!(riscv(&vm, layout, 1, "", &mut buf), Err(Error::Dtb));
    }
    assert_eq!(buf, [0xa3; 64]);
}

#[test]
fn sstc_advertisement_applies_only_to_linux_when_enabled() {
    let contains =
        |haystack: &[u8], needle: &[u8]| haystack.windows(needle.len()).any(|w| w == needle);
    let layout = GuestBoardLayout::DEFAULT;
    let mut out = [0u8; MAX_GUEST_DTB];

    for (aia, plain_isa, sstc_isa) in [
        (
            false,
            b"rv64imafdc_zicsr_zifencei_zicntr\0".as_slice(),
            b"rv64imafdc_zicsr_zifencei_zicntr_sstc\0".as_slice(),
        ),
        (
            true,
            b"rv64imafdc_zicsr_zifencei_zicntr_ssaia\0".as_slice(),
            b"rv64imafdc_zicsr_zifencei_zicntr_ssaia_sstc\0".as_slice(),
        ),
    ] {
        let mut linux = VmConfig::new("linux").unwrap();
        linux.aia = aia;

        let n0 = riscv(&linux, layout, 100_000_000, "console=ttyS0", &mut out).unwrap();
        assert!(contains(&out[..n0], plain_isa));
        assert!(!contains(&out[..n0], b"sstc\0"));

        let n1 =
            riscv_with_sstc(&linux, layout, 100_000_000, "console=ttyS0", true, &mut out).unwrap();
        assert!(contains(&out[..n1], sstc_isa));
        assert!(contains(&out[..n1], b"zicntr\0sstc\0") || contains(&out[..n1], b"ssaia\0sstc\0"));
    }

    let mut fw = VmConfig::new("fw").unwrap();
    fw.memory_size = 64 * 1024;
    fw.image.kernel_size = 4096;
    fw.image.kernel_memory_size = 4096;
    fw.firmware = Some(FirmwareLayout {
        rx_pages: 1,
        ro_pages: 0,
    });
    let n_fw = riscv_with_sstc(&fw, layout, 100_000_000, "", true, &mut out).unwrap();
    assert!(contains(&out[..n_fw], b"rv64imac_zicsr_zifencei_zicntr\0"));
    assert!(!contains(&out[..n_fw], b"sstc\0"));
}

#[test]
fn imsic_node_advertises_the_selected_file_size_only_for_aia() {
    use crate::ImsicIds;
    use pauli_hal::fdt::Fdt;

    let mut vm = VmConfig::new("linux").unwrap();
    let mut buf = [0; MAX_GUEST_DTB];
    for ids in [ImsicIds::Ids127, ImsicIds::Ids255] {
        let layout = GuestBoardLayout {
            imsic_ids: ids,
            ..GuestBoardLayout::DEFAULT
        };
        for aia in [false, true] {
            vm.aia = aia;
            for sstc in [false, true] {
                let len = riscv_with_sstc(&vm, layout, 100_000_000, "", sstc, &mut buf).unwrap();
                let tree = Fdt::new(&buf[..len]).unwrap();
                let node = tree.find("/imsics@28000000").unwrap();
                assert_eq!(node.is_some(), aia);
                if let Some(node) = node {
                    assert_eq!(
                        node.u32("riscv,num-ids").unwrap(),
                        Some(ids.max_id() as u32)
                    );
                }
            }
        }
    }
}
