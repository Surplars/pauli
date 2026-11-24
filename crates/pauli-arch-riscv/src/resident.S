// The resident bank is distinct from the per-exit HS trap envelope. Keep the
// same 15-word layout in the ordinary frame and the dedicated owner's guard.
.macro resident_save base
    csrr t1, hedeleg
    sd t1, BANK_HEDELEG(\base)
    csrr t1, hideleg
    sd t1, BANK_HIDELEG(\base)
    csrr t1, hgatp
    sd t1, BANK_HGATP(\base)
    csrr t1, hvip
    sd t1, BANK_HVIP(\base)
    csrr t1, hcounteren
    sd t1, BANK_HCOUNTEREN(\base)
    csrr t1, henvcfg
    sd t1, BANK_HENVCFG(\base)
    csrr t1, vsstatus
    sd t1, BANK_VSSTATUS(\base)
    csrr t1, vsatp
    sd t1, BANK_VSATP(\base)
    csrr t1, vstvec
    sd t1, BANK_VSTVEC(\base)
    csrr t1, vsscratch
    sd t1, BANK_VSSCRATCH(\base)
    csrr t1, vsepc
    sd t1, BANK_VSEPC(\base)
    csrr t1, vscause
    sd t1, BANK_VSCAUSE(\base)
    csrr t1, vstval
    sd t1, BANK_VSTVAL(\base)
    csrr t1, htimedelta
    sd t1, BANK_HTIMEDELTA(\base)
    csrr t1, hie
    sd t1, BANK_HIE(\base)
.endm

// plan=0 retains the unmanaged/no-VMID full-flush contract. A tagged owner
// restores VMID zero without invalidating another VM's retained translations.
.macro resident_restore base, plan
    csrw vsatp, zero
    ld t1, BANK_HENVCFG(\base)
    csrw henvcfg, t1
    ld t1, BANK_HGATP(\base)
    csrw hgatp, t1
    bnez \plan, .Lresident_g_done\@
    hfence.gvma zero, zero
.Lresident_g_done\@:
    ld t1, BANK_HIDELEG(\base)
    csrw hideleg, t1
    ld t1, BANK_VSSTATUS(\base)
    csrw vsstatus, t1
    ld t1, BANK_VSATP(\base)
    csrw vsatp, t1
    ld t1, BANK_VSTVEC(\base)
    csrw vstvec, t1
    ld t1, BANK_VSSCRATCH(\base)
    csrw vsscratch, t1
    ld t1, BANK_VSEPC(\base)
    csrw vsepc, t1
    ld t1, BANK_VSCAUSE(\base)
    csrw vscause, t1
    ld t1, BANK_VSTVAL(\base)
    csrw vstval, t1
    bnez \plan, .Lresident_v_done\@
    hfence.vvma zero, zero
.Lresident_v_done\@:
    // VSIE/VSIP are base-H aliases. HIE preserves even enables hidden by
    // HIDELEG; HVIP preserves the writable software-pending alias.
    ld t1, BANK_HVIP(\base)
    csrw hvip, t1
    ld t1, BANK_HEDELEG(\base)
    csrw hedeleg, t1
    ld t1, BANK_HIE(\base)
    csrw hie, t1
    ld t1, BANK_HCOUNTEREN(\base)
    csrw hcounteren, t1
    ld t1, BANK_HTIMEDELTA(\base)
    csrw htimedelta, t1
.endm

.option push
.option arch, +h, +d
.section .text
.balign 4
.global pauli_guest_load
.type pauli_guest_load, @function
pauli_guest_load:
    resident_save a0
    ret
.size pauli_guest_load, .-pauli_guest_load

.balign 4
.global pauli_guest_put
.type pauli_guest_put, @function
pauli_guest_put:
    resident_restore a0, a1
    ret
.size pauli_guest_put, .-pauli_guest_put

// a0 = *mut FloatState (host_fp), a1 = *mut usize (host_fs), a2 = *const FloatState (guest_fp)
.balign 4
.global pauli_fp_load
.type pauli_fp_load, @function
pauli_fp_load:
    csrr t0, sstatus
    li t1, FS_DIRTY
    and t0, t0, t1
    sd t0, 0(a1)
    li t1, FP_VECTOR_MASK
    csrc sstatus, t1
    li t1, FS_DIRTY
    csrs sstatus, t1
    fp_save a0, 0
    fp_restore a2, 0
    ret
.size pauli_fp_load, .-pauli_fp_load

// a0 = *mut FloatState (guest_fp), a1 = *const FloatState (host_fp), a2 = usize (host_fs)
.balign 4
.global pauli_fp_put
.type pauli_fp_put, @function
pauli_fp_put:
    li t1, FS_DIRTY
    csrs sstatus, t1
    fp_save a0, 0
    fp_restore a1, 0
    li t1, FS_DIRTY
    csrc sstatus, t1
    and a2, a2, t1
    csrs sstatus, a2
    ret
.size pauli_fp_put, .-pauli_fp_put

// a0 = *const FloatState (host_fp), a1 = usize (host_fs)
.balign 4
.global pauli_fp_restore_host
.type pauli_fp_restore_host, @function
pauli_fp_restore_host:
    li t1, FS_DIRTY
    csrs sstatus, t1
    fp_restore a0, 0
    li t1, FS_DIRTY
    csrc sstatus, t1
    and a1, a1, t1
    csrs sstatus, a1
    ret
.size pauli_fp_restore_host, .-pauli_fp_restore_host
.option pop
