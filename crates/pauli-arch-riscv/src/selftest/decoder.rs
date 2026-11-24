//! Opt-in architecture acceptance fixtures, absent from ordinary boot.
//! Ownership/Lock Ordering: caller owns inactive private guest backing and masked IRQs.
//! Invariants: fixture ranges do not overlap; every load/copy retains private RAM.
use crate::{VCpuContext, VmExit, mmio};
use pauli_hal::addr::PhysAddr;
/// Run the preserved decoder oracle in inactive private R/W/X scratch RAM.
/// # Safety
/// The mapped word is exclusive retained writable/executable RAM, not MMIO.
pub unsafe fn check_mmio_decoder(
    space: &crate::GStagePageTable,
    pc: usize,
) -> Result<(), crate::Error> {
    let word = space
        .translate(PhysAddr::new(pc))
        .ok_or(crate::Error::InvalidImage)?
        .as_ptr::<u32>() as *mut u32;
    // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
    let saved = unsafe { word.read_volatile() };
    let mut ctx = VCpuContext::new();
    ctx.pc = pc;
    ctx.exit_hstatus = 1 << 8;
    ctx.x[11] = 0x1000_0000;
    ctx.x[12] = 0x1234_a5; // SB must pass only the low byte to the device.
    let mut exit = VmExit {
        cause: VmExit::GUEST_STORE_PAGE_FAULT,
        value: 0x1000_0000,
        gpa_shifted: 0x1000_0000 >> 2,
        instruction: 0,
        pc,
        from_guest: true,
    };
    // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
    unsafe {
        byte_accesses(space, pc, word, &mut ctx, &mut exit);
        compressed_accesses(space, pc, word, &mut ctx, &mut exit);
        transformed_accesses(space, pc, word, &mut ctx, &mut exit);
        word.write_volatile(saved);
    }
    Ok(())
}
unsafe fn byte_accesses(
    space: &crate::GStagePageTable,
    pc: usize,
    word: *mut u32,
    ctx: &mut crate::VCpuContext,
    exit: &mut crate::VmExit,
) {
    // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
    unsafe { word.write_volatile(0x00c5_8023) }; // sb a2, 0(a1)
    let mut writes = 0;
    // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
    assert!(unsafe {
        mmio::emulate(ctx, space, *exit, &mut |access| {
            assert_eq!(access.gpa, 0x1000_0000);
            assert_eq!(access.width, 1);
            assert_eq!(access.write, Some(0xa5));
            writes += 1;
            Some(0)
        })
    });
    assert_eq!(writes, 1);
    assert_eq!(ctx.pc, pc + 4);
    ctx.pc = pc;
    // Callback denial must not consume PC or change a guest register.
    // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
    assert!(!unsafe { mmio::emulate(ctx, space, *exit, &mut |_| None) });
    assert_eq!(ctx.pc, pc);
    exit.instruction = 0x3000; // implicit PTE read, never device MMIO
    // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
    assert!(!unsafe { mmio::emulate(ctx, space, *exit, &mut |_| panic!("implicit MMIO")) });
    exit.instruction = 0;
    exit.gpa_shifted += 1; // claimed GPA disagrees with bare effective address
    // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
    assert!(!unsafe { mmio::emulate(ctx, space, *exit, &mut |_| panic!("wrong GPA")) });
    exit.gpa_shifted -= 1;
    exit.cause = VmExit::GUEST_LOAD_PAGE_FAULT;
    for (insn, expected) in [(0x0005_8503, usize::MAX - 127), (0x0005_c503, 128)] {
        // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
        unsafe { word.write_volatile(insn) }; // lb/lbu a0, 0(a1)
        // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
        assert!(unsafe {
            mmio::emulate(ctx, space, *exit, &mut |access| {
                assert_eq!(access.write, None);
                Some(128)
            })
        });
        assert_eq!(ctx.x[10], expected);
        ctx.pc = pc;
    }
}
unsafe fn compressed_accesses(
    space: &crate::GStagePageTable,
    pc: usize,
    word: *mut u32,
    ctx: &mut crate::VCpuContext,
    exit: &mut crate::VmExit,
) {
    ctx.x[2] = ctx.x[11];
    for insn in [0x4188, 0x6188, 0x4502, 0x6502] {
        // C.LW/LD and SP forms
        // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
        unsafe { word.write_volatile(insn) };
        let double = insn & 0x2000 != 0;
        // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
        assert!(unsafe {
            mmio::emulate(ctx, space, *exit, &mut |access| {
                assert_eq!(access.width, if double { 8 } else { 4 });
                Some(0x8000_0000)
            })
        });
        assert_eq!(
            ctx.x[10],
            if double {
                0x8000_0000
            } else {
                0xffff_ffff_8000_0000
            }
        );
        assert_eq!(ctx.pc, pc + 2);
        ctx.pc = pc;
    }
    exit.cause = VmExit::GUEST_STORE_PAGE_FAULT;
    for insn in [0xc190, 0xe190, 0xc032, 0xe032] {
        // C.SW/SD and SP forms
        // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
        unsafe { word.write_volatile(insn) };
        // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
        assert!(unsafe {
            mmio::emulate(ctx, space, *exit, &mut |access| {
                assert_eq!(access.write, Some(0x1234_a5));
                Some(0)
            })
        });
        assert_eq!(ctx.pc, pc + 2);
        ctx.pc = pc;
    }
    ctx.exit_hstatus = 0; // VU MMIO is outside this narrow device path
    // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
    assert!(!unsafe { mmio::emulate(ctx, space, *exit, &mut |_| panic!("VU MMIO")) });
    assert_eq!(ctx.pc, pc);
}
unsafe fn transformed_accesses(
    space: &crate::GStagePageTable,
    pc: usize,
    word: *mut u32,
    ctx: &mut crate::VCpuContext,
    exit: &mut crate::VmExit,
) {
    // Synthetic trusted-trap decoder cases on the target. RAM deliberately
    // contains an illegal instruction; only validated HTINST may describe it.
    ctx.exit_hstatus = 1 << 8;
    // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
    unsafe { word.write_volatile(0) };
    exit.cause = VmExit::GUEST_LOAD_PAGE_FAULT;
    for (htinst, length) in [(0x0000_4503, 4), (0x0000_4501, 2)] {
        exit.instruction = htinst; // lbu a0, transformed rs1/address-offset=0.
        // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
        assert!(unsafe {
            mmio::emulate(ctx, space, *exit, &mut |access| {
                assert_eq!(
                    (access.gpa, access.width, access.write),
                    (0x1000_0000, 1, None)
                );
                Some(0x80)
            })
        });
        assert_eq!((ctx.pc, ctx.x[10]), (pc + length, 0x80));
        ctx.pc = pc;
    }
    for instruction in [
        0x3000,
        0x0000_4503 | (1 << 32),
        0x0000_c503,
        0x0000_7503,
        0x0000_4507,
    ] {
        exit.instruction = instruction;
        // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
        assert!(!unsafe { mmio::emulate(ctx, space, *exit, &mut |_| panic!("invalid HTINST")) });
        assert_eq!(ctx.pc, pc);
    }
}
