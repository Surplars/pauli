//! Exercise the same role, staging and one-shot assignment paths as HS boot.
use super::*;
use crate::assignment::{Arenas, Slots};
use core::mem::MaybeUninit;

#[test]
fn trusted_role_alone_selects_the_manager_and_private_service() {
    let mut src = input(true);
    let (mut linux_tree, mut peer_tree) = ([0; MAX_GUEST_DTB], [0; MAX_GUEST_DTB]);
    src.bootargs = "console=ttyS0 openion,role=manager";
    let code = range(0x8023_0000, 48);
    let h = header(7605976);
    let plan = super::super::config(&src, &h, &mut linux_tree, code, &mut peer_tree).unwrap();
    assert!(!plan.management && plan.count() == 1);
    assert_eq!(plan.vms[0].unwrap().name(), "linux");

    src.manager = true;
    src.bootargs = "console=ttyS0 openion,role=guest owner-secret";
    let result = super::super::config(&src, &h, &mut linux_tree, code, &mut peer_tree);
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
        assert!(super::super::config(&src, &h, &mut linux_tree, bad_code, &mut peer_tree).is_err());
    }
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
    // An actual aligned foreign loan, at the smallest valid Linux geometry.
    let layout = std::alloc::Layout::from_size_align(66 << 20, ALIGN as usize).unwrap();
    // SAFETY: this allocation's exact layout is retained until both iterations finish.
    let base = unsafe { std::alloc::alloc(layout) };
    assert!(!base.is_null());
    for &manager in if cfg!(feature = "guest-control") {
        &[false, true][..]
    } else {
        &[false][..]
    } {
        let input = External {
            manager,
            image,
            initrd: Some(archive),
            ram: range(base as usize, layout.size()),
            bootargs: "private-linux-arg",
        };
        let trees = Box::leak(Box::new([[0; MAX_GUEST_DTB]; 2]));
        let peer_code = &src.peer[..48];
        // SAFETY: all initialized sources are retained, disjoint from the assigned RAM.
        let (plan, images) = unsafe { stage(input, peer_code, trees) }.unwrap();
        let arenas: &'static mut Arenas = Box::leak(Box::new([MaybeUninit::uninit(); 2]));
        // SAFETY: no other handle or reference to the allocated RAM exists;
        // the stage above only inspected its geometry, never its bytes.
        let mut slots = unsafe { Slots::bind(&plan, images, arenas) }.unwrap();
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
    // SAFETY: all GuestRam handles have gone out of scope with their iteration.
    unsafe { std::alloc::dealloc(base, layout) };
}
