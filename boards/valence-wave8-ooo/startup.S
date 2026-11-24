// OpenSBI FW_DYNAMIC enters HS with a0=hart, a1=FDT; preserve both into Rust.
.equ HV_BOOT_SIE, {sie}
.section .text.entry,"ax"
.global _start
.balign 4
_start:
    csrci sstatus, HV_BOOT_SIE
    csrw sie, zero
    bnez a0, .Lpark             // Only the boot hart may clear BSS or take the stack.
    .option push
    .option norelax
    la gp, __global_pointer$
    .option pop
    la sp, __boot_stack
    mv tp, zero
    la t0, __bss_start
    la t1, __bss_end
1:  bgeu t0, t1, 2f
    sd zero, 0(t0)
    addi t0, t0, 8
    j 1b
2:  tail pauli_boot
.Lpark:
    wfi
    j .Lpark
