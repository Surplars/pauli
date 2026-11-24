// Linux exec entry, retaining the kernel-provided userspace stack.
// fp_sleep keeps a sentinel in a D register across timer preemption and a syscall.
.equ NR_NANOSLEEP, 101
.equ SPIN_COUNT, 1000000
.equ FP_SENTINEL, 0x3ff0000000000001
.equ SLEEP_NS, 20000000
.section .text.entry,"ax"
.globl _start
_start:
    .option push
    .option norelax
    la gp, __global_pointer$
    .option pop
    tail probe_entry

.section .text.fp_sleep,"ax"
.globl fp_sleep
fp_sleep:
    .option push
    .option arch,+d
    li t0, FP_SENTINEL
    fmv.d.x ft0, t0
    li t1, SPIN_COUNT
1:  addi t1, t1, -1
    bnez t1, 1b
    la a0, duration
    li a1, 0
    li a7, NR_NANOSLEEP
    ecall
    bnez a0, 2f
    li t0, FP_SENTINEL
    fmv.x.d t1, ft0
    xor a0, t0, t1
2:  ret
    .option pop

.section .rodata.duration,"a"
.balign 8
duration:
    .dword 0, SLEEP_NS
