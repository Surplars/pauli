// Bare entry receives satp in a0 and relocated body GVA in a1. The platform
// identity-maps this small trampoline in VS as well as G-stage. The body uses
// a2=data GVA, a3=value, a4=guest data-PTE GVA, a5=replacement PTE.
.option push
.option norvc
.option norelax
.section .rodata
.balign 4
.global pauli_paging_guest_start
.global pauli_paging_guest_body
.global pauli_paging_guest_report
.global pauli_paging_guest_end
pauli_paging_guest_start:
    csrw satp, a0
    sfence.vma zero, zero
    jr a1
pauli_paging_guest_body:
    sd a3, 0(a2)
    ld s0, 0(a2)
    // Guest owns the VS table and changes its data VA to a different GPA.
    sd a5, 0(a4)
    sfence.vma a2, zero
    lwu s1, 0(a2)
    li a7, 0x10
    li a6, 0
pauli_paging_guest_report:
    ecall
    li a0, 0
    li a1, 0
    li a7, 0x53525354
    li a6, 0
    ecall
    unimp
pauli_paging_guest_end:
.option pop
