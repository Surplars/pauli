// Stackless VS WFI acceptance. a0=delay, a4=SIE, a5=STIE.
// s0=post-WFI progress, s1=handler entries, s2=initial time, s3=scause,
// s4=handler time. The host must advance a trapped WFI exactly once.
.option push
.option norvc
.option norelax
.section .rodata
.balign 4
.global pauli_wfi_guest_start
.global pauli_wfi_guest_wait
.global pauli_wfi_guest_report
.global pauli_wfi_guest_end
pauli_wfi_guest_start:
    lla t0, .Lwfi_irq
    csrw stvec, t0
    csrw sie, a5
    csrs sstatus, a4
    li s0, 0
    li s1, 0
    rdtime s2
    add a0, a0, s2
    li a7, 0x54494d45
    li a6, 0
    ecall
pauli_wfi_guest_wait:
    wfi
    addi s0, s0, 1
    li a7, 0x10
    li a6, 0
pauli_wfi_guest_report:
    ecall
    li a0, 0
    li a1, 0
    li a7, 0x53525354
    li a6, 0
    ecall
    unimp
.balign 4
.Lwfi_irq:
    addi s1, s1, 1
    csrr s3, scause
    rdtime s4
    li a0, -1
    li a7, 0x54494d45
    li a6, 0
    ecall
    sret
pauli_wfi_guest_end:
.option pop
