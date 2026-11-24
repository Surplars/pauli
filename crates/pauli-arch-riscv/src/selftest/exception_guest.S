// Recover three VS page faults without an HS exit. Inputs:
// a0=data GVA, a1=data PTE GVA, a2=RO PTE, a3=RW PTE,
// a5=relocated execute target, a6=execute PTE GVA, a7=RX PTE.
// s0=count, s4=cause bitmap, s6/s7=load tval/epc, s8/s9=store tval/epc,
// s10/s11=instruction tval/epc. All code is stackless and position-independent.
.option push
.option norvc
.option norelax
.section .rodata
.balign 4
.global pauli_exception_guest_start
.global pauli_exception_guest_load
.global pauli_exception_guest_store
.global pauli_exception_guest_report
.global pauli_exception_guest_handler
.global pauli_exception_guest_target
.global pauli_exception_guest_end
pauli_exception_guest_start:
    lla t0, pauli_exception_guest_handler
    csrw stvec, t0
    li s0, 0
    li s4, 0
pauli_exception_guest_load:
    ld s2, 0(a0)
    li s3, 0x2468
pauli_exception_guest_store:
    sd s3, 0(a0)
    ld s3, 0(a0)
    jalr ra, 0(a5)
    li a7, 0x10
    li a6, 0
pauli_exception_guest_report:
    ecall
    li a0, 0
    li a1, 0
    li a7, 0x53525354
    li a6, 0
    ecall
    unimp
.balign 4
pauli_exception_guest_handler:
    addi s0, s0, 1
    csrr s1, scause
    li t0, 1
    sll t0, t0, s1
    or s4, s4, t0
    csrr t0, stval
    csrr t1, sepc
    li t2, 13
    beq s1, t2, .Lrepair_load
    li t2, 15
    beq s1, t2, .Lrepair_store
    li t2, 12
    beq s1, t2, .Lrepair_instruction
.Lbad_exception:
    j .Lbad_exception
.Lrepair_load:
    mv s6, t0
    mv s7, t1
    sd a2, 0(a1)
    sfence.vma a0, zero
    sret
.Lrepair_store:
    mv s8, t0
    mv s9, t1
    sd a3, 0(a1)
    sfence.vma a0, zero
    sret
.Lrepair_instruction:
    mv s10, t0
    mv s11, t1
    sd a7, 0(a6)
    sfence.vma a5, zero
    sret
pauli_exception_guest_target:
    ret
.global pauli_exception_guest_vu_entry
.global pauli_exception_guest_vu_spin
pauli_exception_guest_vu_entry:
    csrw sepc, a0
    li t0, 0x100
    csrc sstatus, t0
    sret
pauli_exception_guest_vu_spin:
    addi s0, s0, 1
    j pauli_exception_guest_vu_spin
pauli_exception_guest_end:
.option pop
