// RV64-only. All context/frame operands are injected from Rust layout.
// Each call returns to the trusted host stack; no global current-vCPU pointer.
.option push
.option arch, +h, +d
.section .text
.global pauli_guest_enter
.type pauli_guest_enter, @function
.balign 4
pauli_guest_enter:
    // 384 bytes integer/CSR state + 264 bytes FP + 8 padding, 16-byte aligned.
    // F/D instructions below are reached only by a provisioned F/D vCPU.
    addi sp, sp, -HOST_BYTES
    sd ra, HOST_RA(sp)
    sd s0, HOST_S0(sp)
    sd s1, HOST_S1(sp)
    sd s2, HOST_S2(sp)
    sd s3, HOST_S3(sp)
    sd s4, HOST_S4(sp)
    sd s5, HOST_S5(sp)
    sd s6, HOST_S6(sp)
    sd s7, HOST_S7(sp)
    sd s8, HOST_S8(sp)
    sd s9, HOST_S9(sp)
    sd s10, HOST_S10(sp)
    sd s11, HOST_S11(sp)
    sd gp, HOST_GP(sp)
    sd tp, HOST_TP(sp)
    sd a0, HOST_A0(sp)
    sd a1, HOST_MODE(sp)             // FULL=0, resident LOAD=1, RESUME=2.
    csrr t1, sstatus
    sd t1, HOST_SSTATUS(sp)
    csrr t1, stvec
    sd t1, HOST_STVEC(sp)
    csrr t1, sscratch
    sd t1, HOST_SSCRATCH(sp)
    csrr t1, sie
    sd t1, HOST_SIE(sp)
    csrr t1, sepc
    sd t1, HOST_SEPC(sp)
    csrr t1, hstatus
    sd t1, HOST_HSTATUS(sp)
    bnez a1, .Lhost_bank_saved
    addi t3, sp, HOST_BANK
    resident_save t3
.Lhost_bank_saved:
    csrr t1, scause
    sd t1, HOST_SCAUSE(sp)
    csrr t1, stval
    sd t1, HOST_STVAL(sp)
    csrr t1, htval
    sd t1, HOST_HTVAL(sp)
    csrr t1, htinst
    sd t1, HOST_HTINST(sp)
    csrr t1, scounteren
    sd t1, HOST_SCOUNTEREN(sp)
    sd sp, CTX_HOST_SP(a0)
    // Save even when host FS is Off: dormant host bits must neither leak into
    // the guest nor be overwritten. FS and VS are independent HS access gates.
    // Resident mode (a1 != 0) keeps guest F/D registers live across quick exits.
    li t1, FP_VECTOR_MASK
    csrc sstatus, t1
    ld t2, CTX_FP_ENABLED(a0)
    beqz t2, .Lfp_enter_done
    li t1, FS_DIRTY
    csrs sstatus, t1
    bnez a1, .Lfp_enter_done
    fp_save sp, HOST_FP
    fp_restore a0, CTX_FP
.Lfp_enter_done:
    swap_senvcfg CTX_SENVCFG(a0), HOST_SENVCFG(sp)
    ld t1, CTX_SCOUNTEREN(a0)
    csrw scounteren, t1
    // A short exit does not relinquish the VS bank. Software changed only
    // GPR/PC or pending levels; the remaining snapshot is observational.
    li t2, RESIDENT_RESUME
    beq a1, t2, .Lresident_resume
    // Host policy delegates only ordinary guest exceptions. SBI, virtual instructions
    // and G-stage faults stay in HS regardless of guest policy. Physical STIP
    // is never delegated, so guest SIE/STIE cannot defeat preemption.
    ld t1, CTX_HEDELEG(a0)
    li t2, DELEGATED_EXCEPTIONS
    and t1, t1, t2
    csrw hedeleg, t1
    // Timer delegation is default; external IRQs are explicitly enabled by
    // the owning backend. Never inherit host virtual pending/delegation bits.
    ld t1, CTX_HIDELEG(a0)
    li t2, VIRTUAL_PENDING
    and t1, t1, t2
    csrw hideleg, t1
    // Only TIME is readable by VS. Cycle/instret/performance counters remain
    // trapped. htimedelta=0 defines a shared, continuously advancing clock.
    li t1, COUNTER_TIME
    csrw hcounteren, t1
    // Prevent speculative VS translations from crossing an HGATP/VMID or
    // HENVCFG change. Host VSATP was saved before this point.
    csrw vsatp, zero
    // Keep Sstc disabled: no direct vstimecmp path bypasses SBI emulation.
    csrw henvcfg, zero
    csrw htimedelta, zero
    ld t1, CTX_HGATP(a0)
    csrw hgatp, t1
    ld t2, CTX_TLB_FLAGS(a0)             // 0=legacy, 1=warm, 3=cold/reuse, 5=sibling switch.
    beqz t2, .Lentry_global_g
    andi t2, t2, TLB_INVALIDATE
    beqz t2, .Lentry_g_done
    srli t1, t1, VMID_SHIFT
    li t2, VMID_MASK
    and t1, t1, t2
    // A non-x0 rs2 selects this VMID, including its combined translations.
    hfence.gvma zero, t1
    j .Lentry_g_done
.Lentry_global_g:
    hfence.gvma zero, zero
.Lentry_g_done:
    ld t1, CTX_VSSTATUS(a0)
    csrw vsstatus, t1
    ld t1, CTX_VSATP(a0)
    csrw vsatp, t1
    ld t1, CTX_VSTVEC(a0)
    csrw vstvec, t1
    ld t1, CTX_VSSCRATCH(a0)
    csrw vsscratch, t1
    ld t1, CTX_VSEPC(a0)
    csrw vsepc, t1
    ld t1, CTX_VSCAUSE(a0)
    csrw vscause, t1
    ld t1, CTX_VSTVAL(a0)
    csrw vstval, t1
    ld t1, CTX_VSIE(a0)
    csrw vsie, t1
    ld t1, CTX_TLB_FLAGS(a0)
    li t2, TLB_TAGGED
    beq t1, t2, .Lentry_v_done
    // Target tag is active: discard VS-only entries on first use/reuse OR
    // switching virtual harts (their ASIDs can name different address spaces).
    hfence.vvma zero, zero
.Lentry_v_done:
.Lresident_resume:
    // Timer/device/IPI updates are live even when the rest of the bank stays
    // resident. Keep the raw fixture's VSIP contract as well; managed pending
    // state keeps this writable SSIP alias coherent with HVIP.VSSIP.
    ld t2, CTX_HIDELEG(a0)
    andi t2, t2, VIRTUAL_PENDING
    ld t1, CTX_HVIP(a0)
    and t1, t1, t2
    csrw hvip, t1
    ld t1, CTX_VSIP(a0)
    csrw vsip, t1
    ld t1, CTX_PC(a0)
    csrw sepc, t1
    // SPV enters virtualization. Restore VS/VU from trusted exit state; HS
    // sstatus.SPP, not guest vsstatus.SPP, determines the SRET target mode.
    // VTW traps
    // guest WFI back to HS instead of lending the physical hart to guest idle.
    ld t1, HOST_HSTATUS(sp)
    // Never lend a host IMSIC guest-file selection to an unprovisioned guest.
    // Clearing VGEIN reuses the existing mask sequence, with no extra CSR IO.
    li t2, HSTATUS_FILE_PRIVILEGE_MASK
    not t2, t2
    and t1, t1, t2
    .if {guest_file_enabled}
    // Only the exclusive file-cache owner writes this trusted context byte.
    lbu t2, CTX_GUEST_FILE(a0)
    slli t2, t2, HSTATUS_VGEIN_SHIFT
    or t1, t1, t2
    .endif
    li t2, HSTATUS_GUEST_BASE
    or t1, t1, t2
    ld t2, CTX_RESUME_VS(a0)
    slli t2, t2, PRIVILEGE_SHIFT
    or t1, t1, t2
    csrw hstatus, t1
    // Physical timer and host software kicks return to HS. Guest software
    // interrupts use delegated VSSIP instead; never confuse the two levels.
    li t1, HOST_INTERRUPTS
    csrw sie, t1
    csrr t1, sstatus
    li t2, ~SSTATUS_SPP
    and t1, t1, t2
    ld t2, CTX_RESUME_VS(a0)
    slli t2, t2, PRIVILEGE_SHIFT
    or t1, t1, t2
    csrw sstatus, t1
    la t1, .Lguest_exit
    csrw stvec, t1
    csrw sscratch, a0
    // Restore base t0 last, after guest sp/gp/tp and every other register.
    mv t0, a0
    ld x1, CTX_X1(t0)
    ld x2, CTX_X2(t0)
    ld x3, CTX_X3(t0)
    ld x4, CTX_X4(t0)
    ld x7, CTX_X7(t0)
    ld x8, CTX_X8(t0)
    ld x9, CTX_X9(t0)
    ld x10, CTX_X10(t0)
    ld x11, CTX_X11(t0)
    ld x12, CTX_X12(t0)
    ld x13, CTX_X13(t0)
    ld x14, CTX_X14(t0)
    ld x15, CTX_X15(t0)
    ld x16, CTX_X16(t0)
    ld x17, CTX_X17(t0)
    ld x18, CTX_X18(t0)
    ld x19, CTX_X19(t0)
    ld x20, CTX_X20(t0)
    ld x21, CTX_X21(t0)
    ld x22, CTX_X22(t0)
    ld x23, CTX_X23(t0)
    ld x24, CTX_X24(t0)
    ld x25, CTX_X25(t0)
    ld x26, CTX_X26(t0)
    ld x27, CTX_X27(t0)
    ld x28, CTX_X28(t0)
    ld x29, CTX_X29(t0)
    ld x30, CTX_X30(t0)
    ld x31, CTX_X31(t0)
    // Last trusted scratch before SRET. Residency includes this short tail
    // and the exit's t0/t1 preservation, not just guest instructions.
    ld x6, CTX_X6(t0)
    ld t0, CTX_X5(t0)
    sret
