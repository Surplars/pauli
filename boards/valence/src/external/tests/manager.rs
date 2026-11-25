//! Exercise the same role, staging and one-shot assignment paths as HS boot.
use super::*;
use crate::assignment::{Arenas, Slots, bind_pool};
use core::mem::MaybeUninit;

#[test]
fn trusted_role_alone_selects_the_manager_and_private_service() {
    let mut src = input(true);
    let (mut linux_tree, mut peer_tree) = ([0; MAX_GUEST_DTB], [0; MAX_GUEST_DTB]);
    src.bootargs = "console=ttyS0 openion,role=manager";
    let code = range(0x8023_0000, 48);
    let h = header(7605976);
    let plan = super::super::config(
        &src,
        &h,
        &mut linux_tree,
        code,
        &mut peer_tree,
        GuestBoardLayout::DEFAULT,
    )
    .unwrap();
    assert!(!plan.management && plan.count() == 1);
    assert!(!plan.boot_stats_for(0));
    assert!(!plan.vms[0].unwrap().control_events);
    assert_eq!(plan.vms[0].unwrap().name(), "linux");

    src.manager = true;
    src.bootargs = "console=ttyS0 openion,role=guest owner-secret";
    let result = super::super::config(
        &src,
        &h,
        &mut linux_tree,
        code,
        &mut peer_tree,
        GuestBoardLayout::DEFAULT,
    );
    if !cfg!(feature = "guest-control") {
        assert!(matches!(result, Err(HvError::Unsupported)));
        return;
    }
    let plan = result.unwrap();
    let plan = BootConfig::decode(&plan.encode().unwrap()).unwrap();
    let [Some(linux), Some(peer), None, None] = plan.vms else {
        panic!("wrong slots")
    };
    assert!(plan.management && linux.autostart && !peer.autostart);
    assert!(plan.boot_stats_for(0));
    assert!((1..=pauli_config::MAX_BOOT_VMS).all(|slot| !plan.boot_stats_for(slot)));
    assert!(!peer.host_stats);
    assert!(linux.control_events && !peer.control_events);
    for (vm, bytes) in [(linux, &linux_tree), (peer, &peer_tree)] {
        pauli_config::guest_dtb::validate_control_events(
            &bytes[..vm.dtb_size as usize],
            &vm,
            GuestBoardLayout::DEFAULT,
        )
        .unwrap();
    }
    assert_eq!((linux.name(), peer.name()), ("manager", "service"));
    assert_eq!(
        (linux.host_base, linux.memory_size),
        (0xa000_0000, 510 << 20)
    );
    assert_eq!((peer.host_base, peer.memory_size), (0xbfe0_0000, 64 << 10));
    assert_eq!(linux.guest_base, peer.guest_base);
    assert_eq!(
        peer.firmware,
        Some(FirmwareLayout {
            rx_pages: 1,
            ro_pages: 0
        })
    );
    assert_eq!(peer.image.kernel_source, code.base() as u64);
    assert_eq!((peer.image.initrd_source, peer.image.initrd_size), (0, 0));
    for vm in [linux, peer] {
        assert!(vm.disk.is_none() && vm.net_peer.is_none() && !vm.net_uplink);
        vm.validate_placement(1).unwrap();
    }
    let fdt = Fdt::new(&peer_tree[..peer.dtb_size as usize]).unwrap();
    assert!(
        fdt.find("/")
            .unwrap()
            .unwrap()
            .compatible("openion,firmware-guest")
            .unwrap()
    );
    let chosen = fdt.find("/chosen").unwrap().unwrap();
    assert_eq!(chosen.text("bootargs").unwrap(), Some(""));
    assert!(chosen.property("linux,initrd-start").unwrap().is_none());
    assert!(chosen.property("linux,initrd-end").unwrap().is_none());
    assert_eq!(
        fdt.find("/memory@80000000")
            .unwrap()
            .unwrap()
            .reg()
            .unwrap(),
        Some((peer.guest_base, peer.memory_size))
    );
    for bad_code in [
        range(code.base() + 1, 48),
        range(code.base(), PAGE_SIZE + 1),
    ] {
        assert!(
            super::super::config(
                &src,
                &h,
                &mut linux_tree,
                bad_code,
                &mut peer_tree,
                GuestBoardLayout::DEFAULT
            )
            .is_err()
        );
    }
}

#[cfg(feature = "guest-control")]
#[test]
fn fixed_pair_is_reciprocal_in_both_manifest_and_guest_trees() {
    let layout = GuestBoardLayout::DEFAULT;
    for enabled in [false, true] {
        let mut src = input(false);
        src.manager = true;
        src.peer_net = enabled;
        let [mut manager_tree, mut service_tree] = [[0; MAX_GUEST_DTB]; 2];
        let boot = super::super::config(
            &src,
            &header(7605976),
            &mut manager_tree,
            range(0x8023_0000, 48),
            &mut service_tree,
            layout,
        )
        .unwrap();
        let published = BootConfig::decode(&boot.encode().unwrap()).unwrap();
        for (slot, tree) in [&manager_tree, &service_tree].into_iter().enumerate() {
            let vm = published.vms[slot].unwrap();
            assert_eq!(vm.net_peer, enabled.then_some((1 - slot) as u8));
            assert!(vm.disk.is_none() && !vm.net_uplink);
            let fdt = Fdt::new(&tree[..vm.dtb_size as usize]).unwrap();
            let port = fdt.find("/virtio@10002000").unwrap();
            assert_eq!(port.is_some(), enabled);
            if let Some(port) = port {
                assert!(port.compatible("virtio,mmio").unwrap());
                assert_eq!(
                    port.reg().unwrap(),
                    Some((layout.net.base, layout.net.size))
                );
                assert_eq!(port.u32("interrupts").unwrap(), Some(layout.net_irq));
            }
        }
    }
}

#[cfg(feature = "guest-control")]
#[test]
fn aligned_manager_leaves_only_the_firmware_service_in_the_trailing_two_mib() {
    let mut input = input(false);
    let gib = 1usize << 30;
    input.manager = true;
    input.ram = range(0x1_0000_0000, gib + ALIGN as usize);
    let mut tree = [0; MAX_GUEST_DTB];
    let boot = config(&input, &header(7605976), &mut tree).unwrap();
    let [Some(linux), Some(service), None, None] = boot.vms else {
        panic!("expected two VMs")
    };
    assert_eq!(
        (linux.guest_base, linux.host_base, linux.memory_size),
        (0x8000_0000, 0x1_0000_0000, gib as u64)
    );
    assert!(GuestBoardLayout::DEFAULT.admits_ram(linux.guest_base, linux.memory_size));
    assert_eq!(service.host_base, linux.host_base + linux.memory_size);
    assert_eq!(service.memory_size, 64 << 10);
    assert!(service.host_base + service.memory_size <= input.ram.end() as u64);
    assert!(!service.aia && !service.autostart);
    let fdt = Fdt::new(&tree[..linux.dtb_size as usize]).unwrap();
    assert_eq!(
        fdt.find("/memory@80000000")
            .unwrap()
            .unwrap()
            .reg()
            .unwrap(),
        Some((0x8000_0000, gib as u64))
    );
}

#[test]
fn slots_lend_only_their_own_restart_images_ram_and_arena() {
    // Keep both read-only templates and the archive together; never treat the
    // numeric template envelope, or the unused tail of the grant, as a slice.
    #[repr(C, align(4096))]
    struct Sources {
        image: [u8; PAGE_SIZE],
        archive: [u8; PAGE_SIZE],
        peer: [u8; PAGE_SIZE],
    }
    let mut backing = Box::new(Sources {
        image: [0; PAGE_SIZE],
        archive: [0x5a; PAGE_SIZE],
        peer: [0x71; PAGE_SIZE],
    });
    backing.image[..HEADER_BYTES].copy_from_slice(&header(PAGE_SIZE as u64 + 64));
    let src: &'static Sources = Box::leak(backing);
    let image = range(src.image.as_ptr() as usize, PAGE_SIZE);
    let archive = range(src.archive.as_ptr() as usize, 31);
    // The 66 MiB boot loan and minimum-sized pool have a two MiB gap on each side of the pool.
    let layout = std::alloc::Layout::from_size_align(72 << 20, ALIGN as usize).unwrap();
    // SAFETY: this allocation's exact layout is retained until both iterations finish.
    let base = unsafe { std::alloc::alloc(layout) };
    assert!(!base.is_null());
    let pool_range = range(base as usize + (68 << 20), 2 << 20);
    // SAFETY: both guard pages are inside our allocation and outside all loans.
    unsafe {
        (pool_range.base() as *mut u8)
            .sub(PAGE_SIZE)
            .write_bytes(0xa5, PAGE_SIZE);
        (pool_range.end() as *mut u8).write_bytes(0x5a, PAGE_SIZE);
    }
    for &(manager, has_pool) in if cfg!(feature = "guest-control") {
        &[(false, false), (true, false), (true, true)][..]
    } else {
        &[(false, false)][..]
    } {
        let input = External {
            manager,
            aia: false,
            peer_net: false,
            image,
            initrd: Some(archive),
            ram: range(base as usize, 66 << 20),
            pool: has_pool.then_some(pool_range),
            bootargs: "private-linux-arg",
        };
        let trees = Box::leak(Box::new([[0; MAX_GUEST_DTB]; 2]));
        let peer_code = &src.peer[..48];
        // SAFETY: all initialized sources are retained, disjoint from the assigned RAM.
        let (plan, images) =
            unsafe { stage(input, peer_code, trees, GuestBoardLayout::DEFAULT) }.unwrap();
        let arenas: &'static mut Arenas = Box::leak(Box::new([MaybeUninit::uninit(); 2]));
        // SAFETY: no other handle or reference to the allocated RAM exists;
        // the stage above only inspected its geometry, never its bytes.
        let mut slots = unsafe { Slots::bind(&plan, images, arenas) }.unwrap();
        // SAFETY: this separate allocation range is exclusively ours until pool drops.
        let pool = unsafe { bind_pool(input.pool) }.unwrap();
        assert_eq!(pool.is_some(), has_pool);
        assert!(slots.take(2).is_err());
        let peer = slots.take(1);
        let mut linux = slots.take(0).unwrap();
        assert!(slots.take(0).is_err() && slots.take(1).is_err());
        assert_eq!(linux.images.kernel.as_ptr(), src.image.as_ptr());
        assert_eq!(linux.images.initrd, &src.archive[..31]);
        assert_eq!(linux.ram.host_range().base(), base as usize);
        assert_eq!(linux.ram.len(), (if manager { 64 } else { 66 }) << 20);
        let linux_arena = linux.arena.take(1, 1).unwrap().as_ptr() as usize;
        if manager {
            let mut peer = peer.unwrap();
            assert_eq!(peer.ram.host_range().base(), linux.ram.host_range().end());
            assert_eq!(peer.ram.len(), 64 << 10);
            assert!(!pool_range.overlaps(linux.ram.host_range()));
            assert!(!pool_range.overlaps(peer.ram.host_range()));
            assert_eq!(peer.images.kernel.as_ptr(), peer_code.as_ptr());
            assert!(peer.images.initrd.is_empty());
            assert_eq!(
                peer.images.dtb.as_ptr() as usize - linux.images.dtb.as_ptr() as usize,
                MAX_GUEST_DTB
            );
            assert_eq!(
                peer.arena.take(1, 1).unwrap().as_ptr() as usize - linux_arena,
                64 << 10
            );
        } else {
            assert!(peer.is_err());
        }
    }
    // SAFETY: every pool and guest handle is gone; the guard pages remain initialized.
    unsafe {
        let left =
            core::slice::from_raw_parts((pool_range.base() - PAGE_SIZE) as *const u8, PAGE_SIZE);
        let right = core::slice::from_raw_parts(pool_range.end() as *const u8, PAGE_SIZE);
        assert!(left.iter().all(|&b| b == 0xa5) && right.iter().all(|&b| b == 0x5a));
    }
    // SAFETY: all GuestRam handles have gone out of scope with their iteration.
    unsafe { std::alloc::dealloc(base, layout) };
}
