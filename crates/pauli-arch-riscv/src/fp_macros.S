// FLEN=64 preserves all raw bits, including F NaN boxes. Integer-ABI helpers;
// caller has enabled HS FS and supplied trusted, aligned host memory. Temporary
// t1 is clobbered. Guest FS may be Off or forged Clean: never use it to skip saves.
.macro fp_save base, offset
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        fsd f\n, (\offset + FP_WORD * \n)(\base)
    .endr
    csrr t1, fcsr
    sd t1, (\offset + FP_FCSR)(\base)
.endm
.macro fp_restore base, offset
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        fld f\n, (\offset + FP_WORD * \n)(\base)
    .endr
    ld t1, (\offset + FP_FCSR)(\base)
    csrw fcsr, t1
.endm
