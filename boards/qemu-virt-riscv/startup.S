// OpenSBI enters once in S-mode with a0=hart and a1=FDT. Workers use SBI HSM.
.equ HV_BOOT_SIE, {sie}
.section .text.entry,"ax"
.global _start
.balign 4
_start:
    csrci sstatus, HV_BOOT_SIE
    csrw sie, zero
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
