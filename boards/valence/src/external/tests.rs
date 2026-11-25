//! The native ranges describe ownership; the generated Linux tree must only
//! describe guest addresses. All tests run without dereferencing board memory.
use super::*;
use pauli_config::{ALIGN, KERNEL_OFFSET, linux_image::HEADER_BYTES};
use pauli_hal::fdt::Fdt;

fn range(base: usize, size: usize) -> PhysRange {
    PhysRange::new(base, size).unwrap()
}
fn input(archive: bool) -> External<'static> {
    External {
        manager: false,
        aia: false,
        peer_net: false,
        image: range(0x8900_0000, 7282696),
        initrd: archive.then(|| range(0x8100_0000, 0x074a970b)),
        ram: range(0xa000_0000, 512 << 20),
        pool: None,
        bootargs: "console=ttyS0,115200 rdinit=/init",
    }
}
fn config(input: &External<'_>, h: &[u8], out: &mut [u8]) -> Result<BootConfig> {
    super::config(
        input,
        h,
        out,
        range(0x8022_0000, 48),
        &mut [0; MAX_GUEST_DTB],
        GuestBoardLayout::DEFAULT,
    )
}
fn header(loaded: u64) -> [u8; HEADER_BYTES] {
    let mut h = [0; HEADER_BYTES];
    h[8..16].copy_from_slice(&KERNEL_OFFSET.to_le_bytes());
    h[16..24].copy_from_slice(&loaded.to_le_bytes());
    h[56..60].copy_from_slice(b"RSC\x05");
    h
}
#[test]
fn wave8_manifest_and_guest_tree_use_different_address_spaces() {
    for archive in [false, true] {
        let src = input(archive);
        let mut dtb = [0; MAX_GUEST_DTB];
        let b = config(&src, &header(7605976), &mut dtb).unwrap();
        let roundtrip = BootConfig::decode(&b.encode().unwrap()).unwrap();
        let vm = roundtrip.vms[0].unwrap();
        assert!(vm.autostart && !vm.aia && vm.disk.is_none() && !vm.net_uplink);
        assert_eq!(vm.vcpus, 1);
        assert_eq!(
            (vm.image.kernel_size, vm.image.kernel_memory_size),
            (7282696, 7605976)
        );
        assert_eq!(vm.image.kernel_source, src.image.base() as u64);
        assert_eq!(
            vm.image.initrd_source,
            src.initrd.map_or(0, |r| r.base() as u64)
        );
        assert_eq!(
            (vm.guest_base + KERNEL_OFFSET, vm.host_base + KERNEL_OFFSET),
            (0x8020_0000, 0xa020_0000)
        );
        let tree = Fdt::new(&dtb[..b.dtb_size as usize]).unwrap();
        let node = |s| tree.find(s).unwrap().unwrap();
        assert_eq!(
            node("/").text("compatible").unwrap(),
            Some("openion,linux-guest")
        );
        assert_eq!(
            node("/memory@80000000").reg().unwrap(),
            Some((vm.guest_base, vm.memory_size))
        );
        assert_eq!(
            node("/cpus").u32("timebase-frequency").unwrap(),
            Some(100_000_000)
        );
        assert!(node("/plic@c000000").compatible("riscv,plic0").unwrap());
        for path in [
            "/soc",
            "/reserved-memory",
            "/cpus/cpu@1",
            "/imsics@28000000",
            "/virtio@10001000",
            "/virtio@10002000",
        ] {
            assert!(tree.find(path).unwrap().is_none());
        }
        let chosen = node("/chosen");
        assert_eq!(chosen.text("bootargs").unwrap(), Some(src.bootargs));
        for (name, offset) in [
            ("linux,initrd-start", vm.initrd_offset()),
            (
                "linux,initrd-end",
                vm.initrd_offset() + vm.image.initrd_size,
            ),
        ] {
            let expected = (vm.guest_base + offset).to_be_bytes();
            assert_eq!(
                chosen.property(name).unwrap(),
                archive.then_some(expected.as_slice())
            );
        }
    }
}

#[test]
fn native_guest_file_requires_an_aia_guest_and_a_physical_file() {
    use crate::handoff::Handoff;
    let mut handoff = Handoff {
        external: None,
        guest_ids: Some(127),
    };
    assert_eq!(handoff.guest_file_ids(), None);
    let mut linux = input(false);
    handoff.external = Some(linux);
    assert_eq!(handoff.guest_file_ids(), None);
    linux.aia = true;
    handoff.external = Some(linux);
    assert_eq!(handoff.guest_file_ids(), Some(127));
    handoff.guest_ids = None;
    assert_eq!(handoff.guest_file_ids(), None);
}

#[test]
fn aligned_linux_grant_maps_as_a_single_root_leaf() {
    use pauli_arch_riscv::gstage::{GStagePageTable, GStageRoot};
    use pauli_hal::addr::PhysAddr;
    let gib = 1usize << 30;
    let host_base = 0x1_0000_0000;
    let mut src = input(false);
    src.ram = range(host_base, gib);
    let boot = config(&src, &header(7605976), &mut [0; MAX_GUEST_DTB]).unwrap();
    let vm = boot.vms[0].unwrap();
    assert_eq!(
        (vm.guest_base, vm.memory_size, vm.host_base),
        (0x8000_0000, gib as u64, host_base as u64)
    );
    assert!(GuestBoardLayout::DEFAULT.admits_ram(vm.guest_base, vm.memory_size));
    let guest = PhysAddr::new(vm.guest_base as usize);
    let host = PhysAddr::new(vm.host_base as usize);
    // SAFETY: private, inactive page-table fixture with no lower table storage.
    // The scalar physical range is never dereferenced or installed on a hart.
    let mut table =
        unsafe { GStagePageTable::new_identity(Box::leak(Box::new(GStageRoot::new())), &mut []) };
    unsafe { table.map_private_ram_1g(guest, host) }.unwrap();
    let last = PhysAddr::new(guest.raw() + gib - 1);
    assert_eq!(
        table.translate(last),
        Some(PhysAddr::new(host.raw() + gib - 1))
    );
    assert_eq!(table.translate(PhysAddr::new(guest.raw() + gib)), None);
}

#[test]
fn aia_linux_uses_the_board_geometry_and_service_stays_on_plic() {
    use pauli_config::ImsicIds;
    for (ids, count) in [(ImsicIds::Ids127, 127), (ImsicIds::Ids255, 255)] {
        let layout = GuestBoardLayout {
            imsic_ids: ids,
            ..GuestBoardLayout::DEFAULT
        };
        let mut src = input(false);
        src.aia = true;
        for manager in [false, true] {
            src.manager = manager;
            let mut tree = [0; MAX_GUEST_DTB];
            let mut peer_tree = [0; MAX_GUEST_DTB];
            let plan = super::config(
                &src,
                &header(7605976),
                &mut tree,
                range(0x8023_0000, 48),
                &mut peer_tree,
                layout,
            );
            if !cfg!(feature = "aia") || (manager && !cfg!(feature = "guest-control")) {
                assert!(matches!(plan, Err(HvError::Unsupported)));
                continue;
            }
            let plan = plan.unwrap();
            let linux = plan.vms[0].unwrap();
            assert!(linux.aia);
            let fdt = Fdt::new(&tree[..linux.dtb_size as usize]).unwrap();
            let imsic = fdt.find("/imsics@28000000").unwrap().unwrap();
            assert_eq!(imsic.u32("riscv,num-ids").unwrap(), Some(count));
            assert!(fdt.find("/plic@c000000").unwrap().is_none());
            if let Some(peer) = plan.vms[1] {
                assert!(manager && !peer.aia);
                let fdt = Fdt::new(&peer_tree[..peer.dtb_size as usize]).unwrap();
                assert!(fdt.find("/plic@c000000").unwrap().is_some());
                assert!(fdt.find("/imsics@28000000").unwrap().is_none());
            } else {
                assert!(!manager);
            }
        }
    }
}

#[test]
fn malformed_or_overlapping_images_do_not_admit_a_vm() {
    let src = input(true);
    let mut dtb = [0; MAX_GUEST_DTB];
    for h in [
        [0; HEADER_BYTES],
        header(src.image.size() as u64 - 1),
        header(src.ram.size() as u64),
    ] {
        assert!(matches!(
            config(&src, &h, &mut dtb),
            Err(HvError::InvalidImage)
        ));
    }
    let without_initrd = input(false);
    assert!(config(&without_initrd, &header(7605976), &mut dtb[..64]).is_err());
    let last = src.ram.size() as u64
        - (src.initrd.unwrap().size() as u64).next_multiple_of(ALIGN)
        - ALIGN
        - KERNEL_OFFSET;
    assert!(config(&src, &header(last), &mut dtb).is_ok());
    assert!(config(&src, &header(last + 1), &mut dtb).is_err());
}

#[test]
fn cold_boot_sources_remain_the_original_exact_loans() {
    // Supply real, stable, page-aligned buffers here in place of physical DDR.
    // The host never needs a reference to the numeric guest RAM range or the gap.
    #[repr(C, align(4096))]
    struct Templates {
        kernel: [u8; PAGE_SIZE],
        initrd: [u8; PAGE_SIZE],
    }
    let mut backing = Box::new(Templates {
        kernel: [0; PAGE_SIZE],
        initrd: [0xa5; PAGE_SIZE],
    });
    backing.kernel[..HEADER_BYTES].copy_from_slice(&header(PAGE_SIZE as u64 + 64));
    let backing: &'static Templates = Box::leak(backing);
    let src = External {
        image: range(backing.kernel.as_ptr() as usize, PAGE_SIZE),
        initrd: Some(range(backing.initrd.as_ptr() as usize, 31)),
        ..input(true)
    };
    // SAFETY: both exact source ranges are initialized storage retained unmodified for this process.
    let (boot, [Some(images), None]) = (unsafe {
        stage(
            src,
            b"unused",
            Box::leak(Box::new([[0; MAX_GUEST_DTB]; 2])),
            GuestBoardLayout::DEFAULT,
        )
    })
    .unwrap() else {
        panic!("ordinary Linux must not see a service")
    };
    assert_eq!(images.kernel.as_ptr(), backing.kernel.as_ptr());
    assert_eq!(images.initrd.as_ptr(), backing.initrd.as_ptr());
    assert_eq!(images.initrd, [0xa5; 31]);
    assert_eq!(images.dtb.len(), boot.dtb_size as usize);
    assert!(boot.vms[0].unwrap().image.kernel_memory_size > images.kernel.len() as u64);
}

#[test]
#[ignore = "set WAVE8_BUNDLE and WAVE8_EXTERNAL_DTB to the local source bundle and staged native DTB"]
fn packaged_external_guest_plan() {
    use std::{fs, io::Read, path::PathBuf};
    let dir = PathBuf::from(std::env::var_os("WAVE8_BUNDLE").expect("WAVE8_BUNDLE"));
    let native =
        fs::read(std::env::var_os("WAVE8_EXTERNAL_DTB").expect("WAVE8_EXTERNAL_DTB")).unwrap();
    let handoff = crate::handoff::validate_blob(0, &native).unwrap();
    let grant = handoff.external.expect("external guest");
    let native_tree = Fdt::new(&native).unwrap();
    let peer_net = native_tree
        .find("/chosen")
        .unwrap()
        .unwrap()
        .unique_property("openion,peer-net")
        .unwrap()
        .is_some();
    assert_eq!(grant.peer_net, peer_net);
    let native_pool = native_tree
        .nodes()
        .map(|node| node.unwrap())
        .find(|node| {
            node.available().unwrap() && node.compatible("openion,wave8-domain-pool").unwrap()
        })
        .map(|node| node.reg().unwrap().unwrap());
    assert_eq!(
        grant.pool.map(|r| (r.base() as u64, r.size() as u64)),
        native_pool
    );
    let mut image = fs::File::open(dir.join("Image")).unwrap();
    assert_eq!(image.metadata().unwrap().len(), grant.image.size() as u64);
    let mut h = [0; HEADER_BYTES];
    image.read_exact(&mut h).unwrap();
    let mut guest_tree = [0; MAX_GUEST_DTB];
    let boot = super::config(
        &grant,
        &h,
        &mut guest_tree,
        range(0x8022_0000, 48),
        &mut [0; MAX_GUEST_DTB],
        handoff.layout(),
    )
    .unwrap();
    let vm = boot.vms[0].unwrap();
    let runtime = u64::from_le_bytes(h[16..24].try_into().unwrap());
    assert_eq!(vm.image.kernel_memory_size, runtime);
    assert_eq!(vm.image.kernel_source, 0x8900_0000);
    let linux_bytes = grant.ram.size() as u64 - if grant.manager { ALIGN } else { 0 };
    assert_eq!(
        (vm.host_base, vm.memory_size),
        (grant.ram.base() as u64, linux_bytes)
    );
    assert_eq!(vm.aia, grant.aia);
    assert_eq!(vm.net_peer, peer_net.then_some(1));
    if grant.manager {
        let service = boot.vms[1].unwrap();
        assert_eq!(
            (service.host_base, service.memory_size),
            (vm.host_base + linux_bytes, 64 << 10)
        );
        assert!(!service.aia);
        assert_eq!(service.net_peer, peer_net.then_some(0));
    }
    assert_eq!(
        (boot.management, vm.name()),
        (
            grant.manager,
            if grant.manager { "manager" } else { "linux" }
        )
    );
    let tree = Fdt::new(&guest_tree[..vm.dtb_size as usize]).unwrap();
    assert_eq!(tree.find("/virtio@10002000").unwrap().is_some(), peer_net);
    assert!(tree.find("/reserved-memory").unwrap().is_none());
    assert!(tree.nodes().all(|node| {
        !node
            .unwrap()
            .compatible("openion,wave8-domain-pool")
            .unwrap()
    }));
    let imsic = tree.find("/imsics@28000000").unwrap();
    assert_eq!(imsic.is_some(), vm.aia);
    if let Some(imsic) = imsic {
        let ids = handoff.layout().imsic_ids.max_id() as u32;
        assert_eq!(imsic.u32("riscv,num-ids").unwrap(), Some(ids));
    }
    let chosen = tree.find("/chosen").unwrap().unwrap();
    assert_eq!(chosen.text("bootargs").unwrap(), Some(grant.bootargs));
    let guest = tree
        .find("/memory@80000000")
        .unwrap()
        .unwrap()
        .reg()
        .unwrap();
    assert_eq!(guest, Some((0x8000_0000, linux_bytes)));
    assert_eq!(
        tree.find("/cpus")
            .unwrap()
            .unwrap()
            .u32("timebase-frequency")
            .unwrap(),
        Some(100_000_000)
    );
    let initrd = grant.initrd.expect("release bundle includes an initrd");
    let start = vm.guest_base + vm.initrd_offset();
    assert_ne!(start, initrd.base() as u64);
    assert_eq!(
        chosen.property("linux,initrd-start").unwrap(),
        Some(&start.to_be_bytes()[..])
    );
    assert_eq!(
        chosen.property("linux,initrd-end").unwrap(),
        Some(&(start + initrd.size() as u64).to_be_bytes()[..])
    );
    println!(
        "Wave8 external plan: file={} memory={} guest_entry={:#x} initrd_gpa={start:#x} guest_dtb_gpa={:#x} timebase={TIMEBASE_HZ}",
        vm.image.kernel_size,
        runtime,
        vm.guest_base + KERNEL_OFFSET,
        vm.guest_base + vm.guest_dtb_offset()
    );
}

mod manager;
