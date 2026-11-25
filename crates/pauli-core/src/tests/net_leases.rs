//! Dynamic leases use actual pool extents, canonical boot images, split rings and retirement.
use super::{
    model::{Arch, Fixture, OWNER, REJECT_NEXT_SPACE},
    net_peer::{carrier, verify_leased_duplex},
    shape::dtb,
};
use crate::{
    HvError,
    board::{Board, BootImages},
    manager::{Budget, leases::LeaseHandle},
    runtime::Dispatcher,
    vm::Vm,
};
use pauli_config::{FirmwareLayout, GuestBoardLayout, MAX_GUEST_DTB, NetLease, VmConfig};
use pauli_control::{ChildName, ChildSpec, VmId};
use pauli_devices::memory::GuestMemory;
use pauli_hal::fdt::Fdt;
use std::{boxed::Box, format, vec};

const RAM: u64 = 1 << 20;
const SPONSOR: Option<VmId> = Some(VmId(1));

fn fixture() -> Fixture {
    Fixture::with_templates(2, [1; 2], 78, |slot, vm| {
        if slot == 1 {
            vm.memory_size = RAM;
            vm.image.kernel_memory_size = 8192;
            vm.firmware = Some(FirmwareLayout {
                rx_pages: 1,
                ro_pages: 1,
            });
        }
        template(vm)
    })
}

fn template(vm: &VmConfig) -> BootImages {
    let mut tree = vec![0; MAX_GUEST_DTB];
    let len = pauli_config::guest_dtb::riscv_configured(
        vm,
        GuestBoardLayout::DEFAULT,
        1_000_000,
        "",
        false,
        &mut tree,
    )
    .unwrap();
    tree.truncate(len);
    BootImages {
        kernel: &[0x31; 64],
        initrd: &[],
        dtb: Box::leak(tree.into_boxed_slice()),
    }
}

fn spec(name: &str, channel: u8, side: u8) -> ChildSpec {
    ChildSpec {
        net_lease: NetLease::new(channel, side),
        ..ChildName::new(name).unwrap().into()
    }
}

fn advertises_net(vm: &Vm<Arch>) -> bool {
    let blob = dtb(vm);
    let fdt = Fdt::new(&blob).unwrap();
    let name = format!("/virtio@{:x}", GuestBoardLayout::DEFAULT.net.base);
    fdt.find(&name).unwrap().is_some()
}

fn join(f: &Fixture, slot: usize, fault: bool) {
    let rt = &f.runtime;
    let vm = rt.manager.vm(slot).unwrap();
    vm.request_stop(fault, false);
    for owner in 0..2 {
        OWNER.with(|o| o.set(owner));
        rt.dispatch(&mut Dispatcher::new()).unwrap();
    }
    rt.manager.service(rt.board);
    assert!(vm.lifecycle.joined());
}

#[test]
fn leased_linux_and_firmware_above_fixed_ports_retain_one_sponsor_charge_until_destroy() {
    let f = fixture();
    let m = &f.runtime.manager;
    let board = f.runtime.board;
    for name in ["fillerA", "fillerB"] {
        m.provision(board, None, 1, ChildName::new(name).unwrap().into())
            .unwrap();
    }
    // Crossing slot four grows the permanent directory once; warm it before measuring leases.
    let warm = m
        .provision(board, None, 1, ChildName::new("warm").unwrap().into())
        .unwrap();
    let epoch = m.vm(warm).unwrap().epoch();
    m.destroy(warm, epoch).unwrap();
    let pages = board.domain_pool().unwrap().free_pages();
    let linux_bytes = m.vm(0).unwrap().config.memory_size;
    let bytes = LeaseHandle::BYTES as u64;
    m.set_budget(0, Some(Budget::from_bytes(2, 2, linux_bytes + RAM + bytes)))
        .unwrap();
    let a = m
        .provision(board, SPONSOR, 0, spec("sender", 2, 0))
        .unwrap();
    let b = m
        .provision(board, SPONSOR, 1, spec("receiver", 2, 1))
        .unwrap();
    assert_eq!((a, b), (4, 5));
    assert_eq!(
        m.budget(0).unwrap().0,
        Budget::from_bytes(2, 2, linux_bytes + RAM + bytes)
    );
    assert!(!advertises_net(&m.vm(0).unwrap()));
    assert!(!advertises_net(&m.vm(1).unwrap()));
    m.start(board, a).unwrap();
    m.start(board, b).unwrap();
    {
        let sender = m.vm(a).unwrap();
        let receiver = m.vm(b).unwrap();
        assert!(advertises_net(&sender) && advertises_net(&receiver));
        let base = receiver.config.guest_base;
        let mut io = receiver.devices.io.lock();
        assert!(!io.ram.write(base, &[1]));
        assert!(!io.ram.write(base + 4096, &[1]));
        assert!(io.ram.write(base + 8192, &[1]));
        assert!(
            !io.ram
                .write(base + receiver.config.guest_dtb_offset(), &[1])
        );
        drop(io);
        verify_leased_duplex(&f, &sender, &receiver);
    }
    join(&f, a, false);
    assert!(!carrier(&f, &m.vm(b).unwrap()));
    let stopped_epoch = m.vm(a).unwrap().epoch();
    assert_eq!(
        m.provision(board, SPONSOR, 1, spec("taken", 2, 0)),
        Err(HvError::Busy)
    );
    m.start(board, a).unwrap();
    assert!(carrier(&f, &m.vm(b).unwrap()));
    assert_eq!(m.destroy(a, stopped_epoch), Err(HvError::Busy));
    join(&f, a, true);
    let pinned = m.vm(a).unwrap();
    let epoch = pinned.epoch();
    assert_eq!(m.destroy(a, epoch), Err(HvError::Busy));
    drop(pinned);
    m.destroy(a, epoch).unwrap();
    assert!(!carrier(&f, &m.vm(b).unwrap()));
    assert_eq!(
        m.budget(0).unwrap().0,
        Budget::from_bytes(1, 1, RAM + bytes)
    );
    assert_eq!(
        m.provision(board, Some(VmId(2)), 1, spec("foreign", 2, 0)),
        Err(HvError::Permission)
    );
    assert_eq!(
        m.provision(board, None, 1, spec("mixed", 2, 0)),
        Err(HvError::Permission)
    );
    let replacement = m
        .provision(board, SPONSOR, 1, spec("newSender", 2, 0))
        .unwrap();
    assert_eq!(replacement, a);
    let epoch = m.vm(a).unwrap().epoch();
    m.destroy(a, epoch).unwrap(); // Never started: still holds the side until removal.
    join(&f, b, false);
    let epoch = m.vm(b).unwrap().epoch();
    m.destroy(b, epoch).unwrap();
    assert_eq!(m.budget(0).unwrap().0, Budget::default());
    assert_eq!(board.domain_pool().unwrap().free_pages(), pages);
}

#[test]
fn rejected_leases_restore_pool_space_and_never_disturb_an_existing_side() {
    let f = fixture();
    let board = f.runtime.board;
    let m = &f.runtime.manager;
    let pool = board.domain_pool().unwrap();
    let initial = pool.free_pages();
    let mut malformed = spec("bad", 0, 0);
    malformed.net_lease = Some(NetLease {
        channel: 64,
        side: 2,
    });
    assert_eq!(
        m.provision(board, SPONSOR, 1, malformed),
        Err(HvError::InvalidConfig)
    );
    assert_eq!(pool.free_pages(), initial);
    let bytes = LeaseHandle::BYTES as u64;
    m.set_budget(0, Some(Budget::from_bytes(2, 2, RAM + bytes - 1)))
        .unwrap();
    assert_eq!(
        m.provision(board, SPONSOR, 1, spec("a", 0, 0)),
        Err(HvError::Exhausted)
    );
    m.set_budget(0, Some(Budget::from_bytes(2, 2, 2 * RAM + bytes)))
        .unwrap();
    REJECT_NEXT_SPACE.with(|s| s.set(true)); // Fails after the independent host channel was allocated.
    assert!(m.provision(board, SPONSOR, 1, spec("a", 0, 0)).is_err());
    assert_eq!(pool.free_pages(), initial);
    assert_eq!(m.budget(0).unwrap().0, Budget::default());
    let a = m.provision(board, SPONSOR, 1, spec("a", 0, 0)).unwrap();
    let one = pool.free_pages();
    REJECT_NEXT_SPACE.with(|s| s.set(true));
    assert!(m.provision(board, SPONSOR, 1, spec("b", 0, 1)).is_err());
    assert_eq!(pool.free_pages(), one);
    assert_eq!(
        m.budget(0).unwrap().0,
        Budget::from_bytes(1, 1, RAM + bytes)
    );
    let b = m.provision(board, SPONSOR, 1, spec("b", 0, 1)).unwrap();
    let epoch = m.vm(b).unwrap().epoch();
    m.destroy(b, epoch).unwrap();
    assert_eq!(pool.free_pages(), one);
    let epoch = m.vm(a).unwrap().epoch();
    m.vm(a).unwrap().fail_join(HvError::Fault);
    assert_eq!(m.destroy(a, epoch), Err(HvError::Busy));
    assert_eq!(
        m.provision(board, SPONSOR, 1, spec("alreadyBound", 0, 0)),
        Err(HvError::Busy)
    );
    assert_eq!(pool.free_pages(), one);
}

#[test]
fn a_lease_requires_rediscoverable_template_even_without_a_shape_change() {
    let f = Fixture::with_config(1, [1; 2], 2, |_, vm| {
        vm.memory_size = RAM;
        vm.firmware = Some(FirmwareLayout {
            rx_pages: 1,
            ro_pages: 0,
        });
    });
    let board = f.runtime.board;
    let pool = board.domain_pool().unwrap();
    let pages = pool.free_pages();
    let m = &f.runtime.manager;
    assert_eq!(
        m.provision(board, None, 1, spec("lease", 3, 1)),
        Err(HvError::InvalidImage)
    );
    assert_eq!(pool.free_pages(), pages);
    assert_eq!(m.vms().count(), 2);
}
