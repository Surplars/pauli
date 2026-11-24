// An RV64 Image-format test fixture, not a Linux kernel or a native HS payload.
// Image RAM includes both stacks; the loader zeroes the unbacked tail on each epoch.
.section .text.entry,"ax"
.balign 4
.global _start
.option push
.option norvc
_start:
    j .Lprimary
    .word 0
    .dword {load_offset}
    .dword __hv_end - _start
    .dword 0
    .word 2, 0
    .dword 0
    .ascii "RISCV\0\0\0"
    .ascii "RSC\005"
    .word 0
.option pop

.Lprimary:
    la sp, __boot_stack
    la t0, vs_mailbox_primary
    j .Lsetup
.balign 4
.global vs_mailbox_secondary_entry
vs_mailbox_secondary_entry:
    la sp, .Lworker_top
    la t0, vs_mailbox_secondary
.Lsetup:
    csrci sstatus, {sie}
    csrw sie, zero
    .option push
    .option norelax
    la gp, __global_pointer$
    .option pop
    jr t0

.section .bss.worker_stack,"aw",@nobits
.balign 16
    .skip 4096
.Lworker_top:
