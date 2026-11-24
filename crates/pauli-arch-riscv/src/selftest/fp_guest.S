// Stackless F/D acceptance. a1 is private guest RAM, never an HS pointer.
// Blocks: 0=first restore, 512=next bank, 1024=restore after a peer ran.
.option push
.option arch, +d
.option norvc
.section .rodata.pauli_fp_guest, "a"
.balign 4
.global pauli_fp_guest_start
.global pauli_fp_guest_report
.global pauli_fp_guest_resume_report
.global pauli_fp_guest_vu
.global pauli_fp_guest_spin
.global pauli_fp_guest_end
pauli_fp_guest_start:
    li t0, 0x6000
    csrs sstatus, t0
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        fsd f\n, (8 * \n)(a1)
    .endr
    csrr t1, fcsr
    sd t1, 256(a1)
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        fld f\n, (512 + 8 * \n)(a1)
    .endr
    ld t1, 768(a1)
    csrw fcsr, t1
    fadd.d f2, f0, f1
    fadd.s f5, f3, f4
    fdiv.s f6, f3, f7
    // Forge Off/Clean after modifying the bank; HS must still save every bit.
    csrc sstatus, t0
    csrs sstatus, a4
pauli_fp_guest_report:
    ecall
    csrs sstatus, t0
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        fsd f\n, (1024 + 8 * \n)(a1)
    .endr
    csrr t1, fcsr
    sd t1, 1280(a1)
pauli_fp_guest_resume_report:
    ecall
    unimp
pauli_fp_guest_vu:
    li t0, 0x6000
    csrs sstatus, t0
    csrw sepc, a0
    li t0, 0x100
    csrc sstatus, t0
    sret
pauli_fp_guest_spin:
    fadd.d f0, f0, f1
    j pauli_fp_guest_spin
pauli_fp_guest_end:
.option pop
