// Stackless VS timer acceptance, copied as position-independent bytes to GPA.
// Inputs: a0=first delay, a2=repeat delay, a3=IRQ count before report,
// a4=sstatus.SIE (0/2), a5=sie.STIE (0/32).
// Outputs: s0=IRQ count, s1=scause, s2=time before initial TIME, s3=IRQ time,
// s4=sip after acknowledgement, s6=first absolute deadline, t3=loop progress.
// This test handler deliberately owns its scratch registers, not a guest ABI.
.option push
.option norvc
.option norelax
.section .rodata
.balign 4
.global pauli_timer_guest_start
.global pauli_timer_guest_report
.global pauli_timer_guest_end
pauli_timer_guest_start:
    lla t0, .Ltimer_irq
    csrw stvec, t0
    csrw sie, a5
    csrs sstatus, a4
    li s0, 0
    rdtime s2
    add a0, a0, s2
    mv s6, a0
    li a7, 0x54494d45
    li a6, 0
    ecall
.Ltimer_wait:
    addi t3, t3, 1
    bltu s0, a3, .Ltimer_wait
    li a7, 0x10
    li a6, 0
pauli_timer_guest_report:
    ecall
    li a0, 0
    li a1, 0
    li a7, 0x53525354
    li a6, 0
    ecall
    unimp
.balign 4
.Ltimer_irq:
    csrr s1, scause
    rdtime s3
    addi s0, s0, 1
    bgeu s0, a3, .Ltimer_disarm
    add a0, s3, a2
    j .Ltimer_ack
.Ltimer_disarm:
    li a0, -1
.Ltimer_ack:
    li a7, 0x54494d45
    li a6, 0
    ecall
    csrr s4, sip
    sret
pauli_timer_guest_end:
.option pop
