//! Decode acceptance; only fixtures allocate foreign pages.
//! Ownership/Lock Ordering: each test owns its page authority.
//! Invariants: denial never changes PC/GPR state or invokes a device.
use super::mmio_fixture::*;
use crate::{VmExit, mmio, vstage};
use pauli_hal::addr::{MemPerms, PAGE_SIZE, PhysAddr};
use std::{cell::Cell, vec};
#[test]
fn hardware_decode() {
    // Deliberately no assigned instruction/PTE RAM. A captured standard
    // transformation describes the already-executed access without RAM fetch.
    let space = MockSpace {
        pages: vec![],
        reads: Cell::new(0),
    };
    let value = 0xfedc_ba98_7654_3280usize;
    for load in [true, false] {
        for funct in 0..if load { 7 } else { 4 } {
            for length in [2, 4] {
                for register in [0, 10] {
                    transformed_case(&space, value, load, funct, length, register);
                }
            }
        }
    }
    assert_eq!(space.reads.get(), 0);
}
#[test]
fn transformed_encodings() {
    let space = MockSpace {
        pages: vec![],
        reads: Cell::new(0),
    };
    let normal = VmExit {
        cause: 21,
        from_guest: true,
        pc: CODE,
        value: DATA,
        gpa_shifted: IO >> 2,
        instruction: 0x2503,
    }; // transformed lw a0
    // The standard reserves the old immediate fields for ignorable future
    // performance hints; they are not the original effective-address offset.
    for hint in [0, 0x1230_0000, 0xfff0_0000] {
        let mut ctx = context(CODE);
        // SAFETY: the private fixture retains checked foreign RAM and supplies a trusted modeled trap.
        assert!(unsafe {
            mmio::emulate(
                &mut ctx,
                &space,
                VmExit {
                    instruction: normal.instruction | hint,
                    ..normal
                },
                &mut |_| Some(37),
            )
        });
        assert_eq!(ctx.x[10], 37);
    }
    for raw in [
        0,
        0x3000,
        0x3020,
        0x2502,
        0x2503 | (1 << 32),
        0x2503 | (1 << 15),
        0x7503,
        0x2507,
        0x250b,
        0x252f,
        0x0020_2023,
    ] {
        let mut ctx = context(CODE);
        let before = ctx.x;
        // SAFETY: the private fixture retains checked foreign RAM and supplies a trusted modeled trap.
        assert!(!unsafe {
            mmio::emulate(
                &mut ctx,
                &space,
                VmExit {
                    instruction: raw,
                    ..normal
                },
                &mut |_| panic!("unsupported transformed MMIO"),
            )
        });
        assert_eq!((ctx.pc, ctx.x), (CODE, before));
    }
}
#[test]
fn transformed_authority() {
    let space = MockSpace {
        pages: vec![],
        reads: Cell::new(0),
    };
    let normal = VmExit {
        cause: 21,
        from_guest: true,
        pc: CODE,
        value: DATA,
        gpa_shifted: IO >> 2,
        instruction: 0x2503,
    }; // transformed lw a0
    for exit in [
        VmExit {
            cause: 13,
            ..normal
        },
        VmExit {
            from_guest: false,
            ..normal
        },
        VmExit {
            pc: CODE + 2,
            ..normal
        },
        VmExit {
            value: DATA + 1,
            ..normal
        },
        VmExit {
            gpa_shifted: usize::MAX,
            ..normal
        },
    ] {
        let mut ctx = context(CODE);
        // SAFETY: the private fixture retains checked foreign RAM and supplies a trusted modeled trap.
        assert!(!unsafe { mmio::emulate(&mut ctx, &space, exit, &mut |_| panic!("invalid trap")) });
    }
    for status in [0, (1 << 8) | (1 << 5)] {
        let mut ctx = context(CODE);
        ctx.exit_hstatus = status;
        // SAFETY: the private fixture retains checked foreign RAM and supplies a trusted modeled trap.
        assert!(!unsafe {
            mmio::emulate(&mut ctx, &space, normal, &mut |_| {
                panic!("wrong privilege/endian")
            })
        });
    }
    let mut ctx = context(CODE);
    let before = ctx.x;
    let mut denied = 0;
    // SAFETY: the private fixture retains checked foreign RAM and supplies a trusted modeled trap.
    assert!(!unsafe {
        mmio::emulate(&mut ctx, &space, normal, &mut |access| {
            denied += 1;
            assert_eq!(access.gpa, IO);
            None // The hardware hint never grants authority over an unassigned GPA.
        })
    });
    assert_eq!((ctx.pc, ctx.x, denied), (CODE, before, 1));
    // Do not use a stale decoded destination/value after switching VM/device.
    for expected in [17, 29] {
        let mut ctx = context(CODE);
        // SAFETY: the private fixture retains checked foreign RAM and supplies a trusted modeled trap.
        assert!(unsafe { mmio::emulate(&mut ctx, &space, normal, &mut |_| Some(expected)) });
        assert_eq!(ctx.x[10], expected);
    }
}
#[test]
fn fetch_and_reject() {
    let mut space = fixture();
    space.pages[4].half(0, 0xa503);
    space.pages[4].half(2, 0x0005); // lw a0,0(a1)
    let (okay, calls, ctx) = emulate(&space, CODE, false, false);
    assert!(okay);
    assert_eq!(calls, 1);
    assert_eq!(ctx.pc, CODE + 4);
    assert_eq!(ctx.x[10], 0xffff_ffff_8765_4321);
    assert_eq!(space.reads.get(), 6); // One code walk + one mandatory data walk.
    assert_eq!(
        // SAFETY: the private fixture retains checked foreign RAM and supplies a trusted modeled trap.
        unsafe { vstage::fetch_instruction(&space, 0, space.pages[4].gpa) },
        Some((0x0005_a503, 4))
    );
    space.reads.set(0);
    space.pages[4].half(0, 0xa023);
    space.pages[4].half(2, 0x00a5); // sw a0,0(a1)
    assert!(emulate(&space, CODE, true, false).0);
    assert_eq!(space.reads.get(), 6);
    space.reads.set(0);
    space.pages[4].half(0, 0x4004); // c.lw s1,0(s0), no upper half needed.
    let (okay, _, ctx) = emulate(&space, CODE, false, false);
    assert!(okay);
    assert_eq!(ctx.pc, CODE + 2);
    assert_eq!(ctx.x[9], 0xffff_ffff_8765_4321);
    assert_eq!(space.reads.get(), 6);
    space.pages[4].half(PAGE_SIZE - 2, 0xa503);
    space.pages[5].half(0, 0x0005); // Separate VS leaf AND noncontiguous HPA.
    space.reads.set(0);
    assert!(emulate(&space, CODE + PAGE_SIZE - 2, false, false).0);
    assert_eq!(space.reads.get(), 9); // Both code pages are independently checked.
    assert_eq!(emulate(&space, CODE, false, true).1, 0); // Implicit faults stay rejected.
    assert!(!emulate(&space, CODE + 1, false, false).0);
    let original = space.pages[3].ptr;
    // Missing data-table assignment: no callback and no unchecked host load.
    space.pages[3].gpa = 0x8400_0000;
    assert_eq!(emulate(&space, CODE, false, false).1, 0);
    space.pages[3].gpa = 0x8100_3000;
    assert_eq!(space.pages[3].ptr, original);
    space.pages[3].pte(
        0,
        vstage::leaf_pte(PhysAddr::new(IO), MemPerms::READ).unwrap(),
    );
    space.pages[4].half(0, 0xa023);
    space.pages[4].half(2, 0x00a5);
    assert_eq!(emulate(&space, CODE, true, false).1, 0); // Read-only VS data leaf.
    space.pages[5].executable = false;
    assert_eq!(emulate(&space, CODE + PAGE_SIZE - 2, false, false).1, 0);
    space.pages[5].executable = true;
    space.pages[2].pte(1, 0); // Second VS code page absent.
    assert_eq!(emulate(&space, CODE + PAGE_SIZE - 2, false, false).1, 0);
    space.pages[4].half(PAGE_SIZE - 2, 0x4004); // Compressed at boundary needs no next page.
    assert!(emulate(&space, CODE + PAGE_SIZE - 2, false, false).0);
    space.pages[4].executable = false;
    assert_eq!(emulate(&space, CODE, false, false).1, 0);
    std::println!(
        "MMIO decode PASS: hardware HTINST uses zero walks, all integer widths/sign/x0/lengths; zero-HTINST fallback keeps code/data walks, independent cross-page permissions and implicit/misaligned/denied rejection"
    );
}

fn transformed_case(
    space: &MockSpace,
    value: usize,
    load: bool,
    funct: usize,
    length: usize,
    register: usize,
) {
    let mut ctx = context(CODE);
    ctx.x[0] = usize::MAX; // x0 is architectural zero, not this slot.
    let raw = if load {
        (funct << 12) | (register << 7) | 3
    } else {
        (register << 20) | (funct << 12) | 0x23
    };
    let exit = VmExit {
        cause: if load { 21 } else { 23 },
        from_guest: true,
        pc: CODE,
        value: DATA,
        gpa_shifted: IO >> 2,
        instruction: if length == 2 { raw & !2 } else { raw },
    };
    let width = 1 << (funct & 3);
    let shift = 64 - width * 8;
    let before = ctx.x;
    let mut calls = 0;
    // SAFETY: the private fixture retains checked foreign RAM and supplies a trusted modeled trap.
    assert!(unsafe {
        mmio::emulate(&mut ctx, space, exit, &mut |access| {
            calls += 1;
            assert_eq!((access.gpa, access.width), (IO, width));
            assert_eq!(
                access.write,
                if load {
                    None
                } else {
                    Some((if register == 0 { 0 } else { before[register] }) << shift >> shift)
                }
            );
            Some(value)
        })
    });
    assert_eq!(calls, 1);
    assert_eq!(ctx.pc, CODE + length);
    assert_eq!(ctx.x[0], usize::MAX);
    if load && register != 0 {
        let expected = if funct & 4 != 0 {
            value << shift >> shift
        } else {
            ((value << shift) as isize >> shift) as usize
        };
        assert_eq!(ctx.x[register], expected);
    } else {
        assert_eq!(ctx.x, before);
    }
}
