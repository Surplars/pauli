.equ PROBE_BYTES, {frame_bytes}
.equ PROBE_FP, {frame_fp}
.equ PROBE_RA, {frame_ra}
.equ PROBE_OBSERVED, {frame_observed}
.equ PROBE_STATUS, {frame_status}
.equ PROBE_STATE, {probe_fp}
.equ PROBE_BEFORE, {probe_before}
.equ PROBE_AFTER, {probe_after}
.equ PROBE_WORD, {fp_word}
.equ PROBE_FCSR, {fp_fcsr}
.equ PROBE_FS, {fs_mask}
.equ PROBE_FS_SHIFT, {fs_shift}
.equ PROBE_FULL, {full}
// FLEN=64 preserves all raw bits, including F NaN boxes. Integer-ABI helpers;
// caller has enabled HS FS and supplied trusted, aligned host memory. Temporary
// t1 is clobbered. Guest FS may be Off or forged Clean: never use it to skip saves.
.macro probe_save base, offset
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        fsd f\n, (\offset + PROBE_WORD * \n)(\base)
    .endr
    csrr t1, fcsr
    sd t1, (\offset + PROBE_FCSR)(\base)
.endm
.macro probe_restore base, offset
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        fld f\n, (\offset + PROBE_WORD * \n)(\base)
    .endr
    ld t1, (\offset + PROBE_FCSR)(\base)
    csrw fcsr, t1
.endm
// Acceptance only: preserve the caller's real bank even when its FS was Off.
// No Rust, scheduler or logger runs while physical FP sentinels are installed.
.option push
.option arch, +d
.section .text.pauli_guest_fp_probe, "ax"
.balign 4
.global pauli_guest_fp_probe
.type pauli_guest_fp_probe, @function
pauli_guest_fp_probe:
    addi sp, sp, -PROBE_BYTES
    sd ra, PROBE_RA(sp)
    sd a2, PROBE_OBSERVED(sp)
    csrr t2, sstatus
    sd t2, PROBE_STATUS(sp)
    li t1, PROBE_FS
    csrs sstatus, t1
    probe_save sp, PROBE_FP
    probe_restore a1, PROBE_STATE
    ld t2, PROBE_STATUS(sp)
    li t1, ~PROBE_FS
    and t2, t2, t1
    slli a3, a3, PROBE_FS_SHIFT
    or t2, t2, a3
    csrw sstatus, t2
    csrr t2, sstatus
    sd t2, PROBE_BEFORE(a2)
    // This probe owns no Resident guard. The former sentinel pointer in a1
    // must not be interpreted as a resident transition mode.
    li a1, PROBE_FULL
    call pauli_guest_enter
    ld a2, PROBE_OBSERVED(sp)
    csrr t2, sstatus
    sd t2, PROBE_AFTER(a2)
    li t1, PROBE_FS
    csrs sstatus, t1
    probe_save a2, PROBE_STATE
    probe_restore sp, PROBE_FP
    ld t1, PROBE_STATUS(sp)
    csrw sstatus, t1
    ld ra, PROBE_RA(sp)
    addi sp, sp, PROBE_BYTES
    ret
.size pauli_guest_fp_probe, .-pauli_guest_fp_probe
.option pop
