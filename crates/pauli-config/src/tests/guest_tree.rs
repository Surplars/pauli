//! Exact output bounds and fail-closed Linux guest geometry.
//! Ownership/Lock Ordering: each test owns its VM config and guarded byte buffer.
//! Invariants: no out-of-bounds output, overlapping mappings or wrapped offsets.
use crate::{Error, GuestBoardLayout, MAX_GUEST_DTB, MAX_GUEST_VCPUS, VmConfig, guest_dtb::riscv};

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
