.balign 4
.Lguest_exit:
    // Preserve t0/t1 BEFORE using a guest register. sscratch is trusted HS
    // state; guest s0 and sp are never used as host pointers.
    csrrw t0, sscratch, t0
    sd t1, CTX_X6(t0)
    csrr t1, sscratch
    sd t1, CTX_X5(t0)
    sd x1, CTX_X1(t0)
    sd x2, CTX_X2(t0)
    sd x3, CTX_X3(t0)
    sd x4, CTX_X4(t0)
    sd x7, CTX_X7(t0)
    sd x8, CTX_X8(t0)
    sd x9, CTX_X9(t0)
    sd x10, CTX_X10(t0)
    sd x11, CTX_X11(t0)
    sd x12, CTX_X12(t0)
    sd x13, CTX_X13(t0)
    sd x14, CTX_X14(t0)
    sd x15, CTX_X15(t0)
    sd x16, CTX_X16(t0)
    sd x17, CTX_X17(t0)
    sd x18, CTX_X18(t0)
    sd x19, CTX_X19(t0)
    sd x20, CTX_X20(t0)
    sd x21, CTX_X21(t0)
    sd x22, CTX_X22(t0)
    sd x23, CTX_X23(t0)
    sd x24, CTX_X24(t0)
    sd x25, CTX_X25(t0)
    sd x26, CTX_X26(t0)
    sd x27, CTX_X27(t0)
    sd x28, CTX_X28(t0)
    sd x29, CTX_X29(t0)
    sd x30, CTX_X30(t0)
    sd x31, CTX_X31(t0)
    ld sp, CTX_HOST_SP(t0)
    // From here on only the trusted host stack is used.
    // VS may clear/forge its own FS after writing FP registers. HS FS remains
    // enabled for an F/D vCPU, so save unconditionally, then restore the host.
    ld t2, CTX_FP_ENABLED(t0)
    beqz t2, .Lfp_exit_done
    fp_save t0, CTX_FP
    fp_restore sp, HOST_FP
.Lfp_exit_done:
    csrr t1, sepc
    sd t1, CTX_PC(t0)
    csrr t1, vsstatus
    sd t1, CTX_VSSTATUS(t0)
    csrr t1, vsatp
    sd t1, CTX_VSATP(t0)
    csrr t1, vstvec
    sd t1, CTX_VSTVEC(t0)
    csrr t1, vsscratch
    sd t1, CTX_VSSCRATCH(t0)
    csrr t1, vsepc
    sd t1, CTX_VSEPC(t0)
    csrr t1, vscause
    sd t1, CTX_VSCAUSE(t0)
    csrr t1, vstval
    sd t1, CTX_VSTVAL(t0)
    csrr t1, vsie
    sd t1, CTX_VSIE(t0)
    csrr t1, vsip
    sd t1, CTX_VSIP(t0)
    csrr t1, scause
    sd t1, CTX_EXIT_CAUSE(t0)
    csrr t1, stval
    sd t1, CTX_EXIT_VALUE(t0)
    csrr t1, htval
    sd t1, CTX_EXIT_GPA_SHIFTED(t0)
    csrr t1, htinst
    sd t1, CTX_EXIT_INSTRUCTION(t0)
    csrr t1, hstatus
    sd t1, CTX_EXIT_HSTATUS(t0)
    srli t1, t1, PRIVILEGE_SHIFT
    andi t1, t1, PRIVILEGE_BIT
    sd t1, CTX_RESUME_VS(t0)
    csrr t1, scounteren
    sd t1, CTX_SCOUNTEREN(t0)
    csrr t1, hvip
    andi t1, t1, VIRTUAL_PENDING
    sd t1, CTX_HVIP(t0)
    swap_senvcfg HOST_SENVCFG(sp), CTX_SENVCFG(t0)
    ld t1, HOST_SCOUNTEREN(sp)
    csrw scounteren, t1
    ld t1, HOST_MODE(sp)
    bnez t1, .Lhost_bank_restored
    addi t3, sp, HOST_BANK
    ld t4, CTX_TLB_FLAGS(t0)
    resident_restore t3, t4
.Lhost_bank_restored:
    ld t1, HOST_HSTATUS(sp)
    csrw hstatus, t1
    ld t1, HOST_SEPC(sp)
    csrw sepc, t1
    ld t1, HOST_SCAUSE(sp)
    csrw scause, t1
    ld t1, HOST_STVAL(sp)
    csrw stval, t1
    ld t1, HOST_HTVAL(sp)
    csrw htval, t1
    ld t1, HOST_HTINST(sp)
    csrw htinst, t1
    ld t1, HOST_SIE(sp)
    csrw sie, t1
    ld t1, HOST_STVEC(sp)
    csrw stvec, t1
    ld t1, HOST_SSCRATCH(sp)
    csrw sscratch, t1
    ld t1, HOST_SSTATUS(sp)
    csrw sstatus, t1
    ld ra, HOST_RA(sp)
    ld s0, HOST_S0(sp)
    ld s1, HOST_S1(sp)
    ld s2, HOST_S2(sp)
    ld s3, HOST_S3(sp)
    ld s4, HOST_S4(sp)
    ld s5, HOST_S5(sp)
    ld s6, HOST_S6(sp)
    ld s7, HOST_S7(sp)
    ld s8, HOST_S8(sp)
    ld s9, HOST_S9(sp)
    ld s10, HOST_S10(sp)
    ld s11, HOST_S11(sp)
    ld gp, HOST_GP(sp)
    ld tp, HOST_TP(sp)
    addi sp, sp, HOST_BYTES
    ret
.size pauli_guest_enter, .-pauli_guest_enter
.option pop
