.option push
.option arch, +h
.section .rodata.hv_aia_guest, "a"
.balign 4
.global pauli_aia_guest_start
.global pauli_aia_guest_read
.global pauli_aia_guest_end
pauli_aia_guest_start:
    // These access VS aliases. A leaked host HVICTL.VTI would trap SIE/SIP.
    csrr a0, 0x150
    csrr a1, sie
    csrr a2, sip
    li t0, 0xa5
    csrw 0x150, t0
    ecall
pauli_aia_guest_read:
    csrr a0, 0x150
    csrr a1, sie
    ecall
pauli_aia_guest_end:
.option pop
