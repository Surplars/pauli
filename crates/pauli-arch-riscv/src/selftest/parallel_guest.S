// Position-independent, stackless two-hart rendezvous. The first entry on
// BOTH harts must return BASE, not a timer: a serialized dispatcher cannot
// satisfy this barrier without first preempting one of them.
.option push
.option arch, +a
.option norvc
.section .rodata
.balign 4
.global pauli_parallel_guest_start
.global pauli_parallel_guest_end
pauli_parallel_guest_start:
    li t0, 1
    sll t0, t0, a0
    amoor.d.aqrl zero, t0, (a1)
1:  ld t0, (a1)
    li t1, 3
    bne t0, t1, 1b
    li a7, 0x10
    li a6, 0
    ecall
// A physical HS kick must first exit to the owner. Only its subsequent virtual
// injection changes VS SIP; guest acknowledgment must not affect another CPU.
2:  csrr t0, sip
    andi t0, t0, 2
    beqz t0, 2b
    csrci sip, 2
    li a7, 0x10
    li a6, 0
    ecall
3:  addi s0, s0, 1
    j 3b
pauli_parallel_guest_end:
.option pop
