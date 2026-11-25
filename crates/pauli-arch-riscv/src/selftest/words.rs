//! Opt-in architecture acceptance fixtures, absent from ordinary boot.
//! Ownership/Lock Ordering: caller owns inactive private guest backing and masked IRQs.
//! Invariants: fixture ranges do not overlap; every load/copy retains private RAM.
use super::images::*;
pub const WORDS: &[(usize, &[u32])] = &[
    (0, &[0x02a0_0513u32, 0x0000_0073, 0x0015_0513, 0x0000_0073]),
    // sd a2,0(a1); ld a0,0(a1); ecall
    (0x40, &[0x00c5_b023, 0x0005_b503, 0x0000_0073]),
    // sd a2,0(a1); ecall (write fault must precede ecall)
    (0x80, &[0x00c5_b023, 0x0000_0073]),
    (0xc0, &[0x0005_b503, 0x0000_0073]),
    // addi t0,t0,1; jal zero,-4 (no cooperative ecall or WFI)
    (0x100, &[0x0012_8293, 0xffdf_f06f]),
    // addi t0,zero,1; csrw scounteren,t0; csrw senvcfg,t0; ecall
    (0x140, &[0x0010_0293, 0x1062_9073, 0x10a2_9073, 0x0000_0073]),
    // First SBI request comes from the prepared registers. After it
    // returns, issue SRST shutdown. The final addi must never execute.
    (
        0x180,
        &[
            0x0000_0073,
            0x0000_0513,
            0x0000_0593,
            0x5352_58b7,
            0x3548_8893,
            0x0000_0813,
            0x0000_0073,
            0x0012_8293,
        ],
    ),
    // rdcycle a0; ecall: TIME access must not grant other counters.
    (0x1c0, &[0xc000_2573, 0x0000_0073]),
    // Three guest stimecmp exchanges, one per entry mode. Each following
    // BASE ecall reports the old comparator in s0, s1 or s2.
    (
        0x1d0,
        &[
            0x14d6_1473,
            0x0000_0073, // csrrw s0,stimecmp,a2; ecall
            0x14d6_94f3,
            0x0000_0073, // csrrw s1,stimecmp,a3; ecall
            0x14d7_1973,
            0x0000_0073, // csrrw s2,stimecmp,a4; ecall
        ],
    ),
    // Masked TIME replacement: initial request, future time from a2,
    // observe sip in s0, disarm, observe sip in s1, BASE report.
    (
        0x400,
        &[
            0x0000_0073, // ecall
            0x0006_0513, // mv a0,a2
            0x0000_0073, // ecall
            0x1440_2473, // csrr s0,sip
            0xfff0_0513, // li a0,-1
            0x0000_0073, // ecall
            0x1440_24f3, // csrr s1,sip
            0x0100_0893, // li a7,BASE
            0x0000_0073, // ecall
        ],
    ),
    // wfi; addi s0,s0,1; ecall (no guest timer, host event only).
    (0x480, &[0x1050_0073, 0x0014_0413, 0x0000_0073]),
    // wfi; addi s0,s0,1; jal zero,-8 (coalesced host kicks).
    (0x4c0, &[0x1050_0073, 0x0014_0413, 0xff9f_f06f]),
    // Repeated BASE ecalls: managed response resumes at the back branch.
    (BENCH_OFFSET, &[0x0000_0073, 0xffdf_f06f]),
    // Bare boot: a1 points to [satp, paged PC, data VA]. Each sibling
    // installs its own root but deliberately uses the same guest ASID.
    (
        VMID_BOOT_OFFSET,
        &[
            0x0005_b283, // ld t0,0(a1)
            0x0085_b303, // ld t1,8(a1)
            0x0105_b603, // ld a2,16(a1)
            0x1802_9073, // csrw satp,t0
            0x1200_0073, // sfence.vma zero,zero
            0x0003_0067, // jr t1
        ],
    ),
    // Repeated data load + BASE report. No SFENCE on warm reentry: the
    // hypervisor, not this fixture, must isolate colliding sibling ASIDs.
    (
        VMID_BODY_OFFSET,
        &[
            0x0006_3403, // ld s0,0(a2)
            0x0100_0893, // li a7,BASE
            0x0000_0813, // li a6,0
            0x0000_0073, // ecall
            0xff1f_f06f, // j -16
        ],
    ),
    // Warm a data translation, replace its guest-owned leaf PTE, report.
    // No SFENCE here: a mixed host RFENCE batch must discard the old VS
    // mapping before the next load. a3=PTE alias, a4=replacement leaf.
    (
        RFENCE_BODY_OFFSET,
        &[
            0x0006_3403, // ld s0,0(a2)
            0x00e6_b023, // sd a4,0(a3)
            0x0100_0893, // li a7,BASE
            0x0000_0813, // li a6,0
            0x0000_0073, // ecall
            0xfedf_f06f, // j -20
        ],
    ),
];
