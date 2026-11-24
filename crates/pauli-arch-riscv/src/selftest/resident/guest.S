// Stackless VS bank fixture. Global SIE stays clear: pending bits are observed
// as data, while the physical HS timer can still preempt this guest.
.option push
.option norvc
.section .rodata.pauli_resident_guest, "a"
.balign 4
.global pauli_resident_guest_start
.global pauli_resident_guest_read
.global pauli_resident_guest_end
pauli_resident_guest_start:
    csrw sscratch, a2
    csrw sepc, a3
    csrw scause, a4
    csrw stval, a5
    csrw stvec, a6
    li t0, 0x40000
    csrs sstatus, t0
    li t0, 0x222
    csrw sie, t0
    ecall
pauli_resident_guest_read:
    csrr a0, sscratch
    csrr a1, sepc
    csrr a2, scause
    csrr a3, stval
    csrr a4, stvec
    csrr a5, sstatus
    csrr a6, sie
    csrr a7, sip
    ecall
pauli_resident_guest_end:
.option pop
