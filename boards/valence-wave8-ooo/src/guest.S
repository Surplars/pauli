// Execute only in the private guest RX page. No stack or firmware pointers.
.equ GUEST_TIMER_CAUSE, {timer_cause}
.section .rodata.wave8_guest,"a"
.option push
.option norelax
.balign 4
.global wave8_guest_start
wave8_guest_start:
    mv s0, a1                  // Native timebase supplied as a scalar.
    li a7, {base}
    li a6, 0
    ecall
    bnez a0, .Lfail
    lla t0, .Ltimer
    csrw stvec, t0
    li t0, {stip}
    csrs sie, t0
    csrsi sstatus, {sie}
    rdtime a0
    li t0, 1000
    divu t0, s0, t0
    add a0, a0, t0
    li a7, {time}
    ecall
    bnez a0, .Lfail
.Lwait:
    wfi
    j .Lwait
.balign 4
.Ltimer:
    csrr t0, scause
    li t1, GUEST_TIMER_CAUSE
    bne t0, t1, .Lfail
    li a0, -1
    li a7, {time}
    li a6, 0
    ecall
    bnez a0, .Lfail
    lla t1, .Lmsg
    j .Lreport
.Lfail:
    lla t1, .Lfailed
.Lreport:
    li t0, {uart}
1:  lbu t2, 0(t1)
    beqz t2, 2f
    sb t2, 0(t0)
    addi t1, t1, 1
    j 1b
2:  li a0, 0
    li a1, 0
    li a7, {srst}
    ecall
.Lhalt:
    wfi
    j .Lhalt
.Lmsg: .asciz "Wave8 guest: BASE/TIME/WFI PASS\n"
.Lfailed: .asciz "Wave8 guest: BASE/TIME/WFI FAIL\n"
.global wave8_guest_end
wave8_guest_end:
.option pop

// A stopped, diskless service for the management-domain example. It speaks
// the firmware+DTB ABI, does not inspect a1, and never needs the Linux image.
.section .rodata.wave8_service,"a"
.option push
.option norelax
.balign 4096
.global wave8_service_start
wave8_service_start:
    li t0, {uart}
    lla t1, .Lservice_msg
1:  lbu t2, 0(t1)
    beqz t2, 2f
    sb t2, 0(t0)
    addi t1, t1, 1
    j 1b
2:  wfi
    j 2b
.Lservice_msg: .asciz "Wave8 service: ready\n"
.global wave8_service_end
wave8_service_end:
.option pop
