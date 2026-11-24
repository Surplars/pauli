// Native S-mode measurement endpoint: HSM supplies a0=hart, a1=retained mailbox.
// No stack, gp, nested traps, timeout polling, UART or SBI calls in the scored loop.
// The sender cannot issue the next IPI until this sample's result was consumed.
.equ SOFT_CAUSE, (1 << 63) | 1
.equ HART_STOP, 1
.section .text
.balign 4
.global native_ipi_worker
native_ipi_worker:
    csrci sstatus, {sie}
    csrw sie, zero
    mv s0, a1
1:  ld t0, {start}(s0)
    beqz t0, 1b
    fence r, rw
    sd a0, {hart}(s0)
    lla t0, .Ltrap
    csrw stvec, t0
    csrci sip, {ssip}
    csrsi sie, {ssip}
    li s1, 0

.Lnext:
    addi s1, s1, 1
    li s2, 0                          // Register-only work in this sequence.
    li t0, {samples}
    bgtu s1, t0, .Lstop
    csrsi sstatus, {sie}
    li t0, {round}
    bleu s1, t0, .Larm_wait
    li t0, 3 * {round}
    bgtu s1, t0, .Larm_wait
    li s3, 1
    j .Lbusy
.Larm_busy:
    fence rw, w
    sd s1, {ready}(s0)                // Publish only after the first loop iteration.
.Lbusy:
    addi s2, s2, 1
    beq s2, s3, .Larm_busy
    j .Lbusy
.Lbusy_end:
.Larm_wait:
    fence rw, w
    sd s1, {ready}(s0)
.Lwfi:
    wfi
.Lafter_wfi:
    j .Lwfi                           // Spurious WFI return is not an SSIP receipt.

.balign 4
.Ltrap:
    rdtime t6                         // Endpoint: first handler instruction in both modes.
    csrr t0, scause
    li t1, SOFT_CAUSE
    bne t0, t1, .Lfail
    csrci sip, {ssip}                 // Acknowledge before releasing DONE to the sender.
    csrr t0, sepc
    lla t1, .Lwfi
    li t2, {wait_armed}
    beq t0, t1, .Lpublish
    lla t1, .Lafter_wfi
    li t2, {wait_resume}
    beq t0, t1, .Lpublish
    lla t1, .Lbusy
    bltu t0, t1, .Lfail
    lla t1, .Lbusy_end
    bgeu t0, t1, .Lfail
    li t2, {busy}
.Lpublish:
    sd t6, {ticks}(s0)
    sd t2, {site}(s0)
    sd s2, {work}(s0)
    fence iorw, w                     // Order TIME input, SSIP clear and RAM stores before DONE.
    sd s1, {done}(s0)
    // Never return to a potentially unexecuted WFI after clearing its SSIP.
    lla t0, .Lnext
    csrw sepc, t0
    sret

.Lstop:
    csrci sstatus, {sie}
    csrw sie, zero
    csrci sip, {ssip}
    li a7, {hsm}
    li a6, HART_STOP
    ecall                             // SBI HSM hart_stop does not return on success.
.Lfail:
    csrci sstatus, {sie}
    csrw sie, zero
    li t0, -1
    fence rw, w
    sd t0, {done}(s0)
    sd t0, {ready}(s0)
2:  wfi
    j 2b
