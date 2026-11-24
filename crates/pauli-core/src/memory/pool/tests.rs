//! Exercise the actual extent list and host-only arena on a small page-aligned bank.
//! One test owns each bank; unpublished receipts return only after borrows end.
use super::*;
use pauli_config::FirmwareLayout;
use std::{boxed::Box, vec};

const ROOT_BYTES: usize = 4 * PAGE_SIZE; // Sv39x4 root table size and alignment.

fn small_bank(bytes: usize) -> (&'static DomainPool, usize) {
    let mem = Box::leak(vec![0u8; bytes + ROOT_BYTES + PAGE_SIZE].into_boxed_slice());
    // Always start one page past a root boundary, never on a Linux 2 MiB boundary.
    let base = (mem.as_mut_ptr() as usize).next_multiple_of(ROOT_BYTES) + PAGE_SIZE;
    let frames = FramePool::new(base, bytes).unwrap();
    // SAFETY: the leaked backing is exclusive to this pool for the test's lifetime.
    (
        Box::leak(Box::new(unsafe { DomainPool::from_boot(frames) })),
        base,
    )
}

fn firmware() -> VmConfig {
    let mut vm = VmConfig::new("fw").unwrap();
    vm.guest_base = PAGE_SIZE as u64;
    vm.memory_size = 8 * PAGE_SIZE as u64;
    vm.firmware = Some(FirmwareLayout {
        rx_pages: 1,
        ro_pages: 0,
    });
    vm
}

fn images() -> BootImages {
    BootImages {
        kernel: &[],
        initrd: &[],
        dtb: &[],
    }
}

#[test]
fn small_firmware_bank_keeps_tables_objects_and_peers_outside_guest_ram() {
    let vm = firmware();
    let bytes = vm.memory_size as usize;
    const META: usize = 64;
    let size = ExtentLayout::new::<u64>(bytes, PAGE_SIZE, META)
        .unwrap()
        .size;
    let (pool, base) = small_bank(2 * size);
    assert_eq!(pool.capacity::<u64>(&vm, META).unwrap(), 2);
    let linux = VmConfig::new("linux").unwrap();
    assert_eq!(pool.capacity::<u64>(&linux, META).unwrap(), 0);
    assert_eq!(ram_alignment(&linux), pauli_config::ALIGN as usize);
    assert!(ExtentLayout::new::<u64>(bytes, ram_alignment(&linux), META).is_err());

    let allocate = || pool.allocate::<u64>(&vm, images(), META).unwrap();
    let first = {
        let (mut child, object, receipt) = allocate();
        assert_eq!(child.ram.host_range().base(), base);
        assert_eq!(child.ram.len(), bytes);
        let root = child.arena.take(ROOT_BYTES, ROOT_BYTES).unwrap().as_ptr() as usize;
        assert!(root > child.ram.host_range().end()); // Real root alignment padding.
        assert_eq!(root % ROOT_BYTES, 0);
        let tables = child.arena.take(4 * PAGE_SIZE, PAGE_SIZE).unwrap();
        assert_eq!(tables.as_ptr() as usize, root + ROOT_BYTES);
        let private = child.arena.take(META, align_of::<u64>()).unwrap().as_ptr() as usize;
        assert!(private + META <= object.as_mut_ptr() as usize);
        receipt
    };
    let second = {
        let (child, _, receipt) = allocate();
        assert_eq!(child.ram.host_range().base(), base + size);
        assert_eq!(pool.capacity::<u64>(&vm, META).unwrap(), 0);
        assert!(matches!(
            pool.allocate::<u64>(&vm, images(), META),
            Err(HvError::Exhausted)
        ));
        receipt
    };
    // SAFETY: neither child was published or mapped, and all subrange borrows ended.
    unsafe {
        first.release();
        second.release();
    }
    assert_eq!(pool.capacity::<u64>(&vm, META).unwrap(), 2);
}

#[test]
fn invalid_guest_range_returns_the_whole_reservation() {
    let mut vm = firmware();
    let size = ExtentLayout::new::<u64>(vm.memory_size as usize, PAGE_SIZE, 0)
        .unwrap()
        .size;
    let (pool, _) = small_bank(size);
    vm.guest_base = u64::MAX - PAGE_SIZE as u64 + 1;
    assert!(matches!(
        pool.allocate::<u64>(&vm, images(), 0),
        Err(HvError::InvalidMemory)
    ));
    assert_eq!(pool.capacity::<u64>(&vm, 0).unwrap(), 1);
}
