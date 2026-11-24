//! Position-independent integer guest for the Wave8 HS handoff.
//! Ownership/Lock Ordering: immutable linker bytes; the core copies into private RX RAM.
//! Invariants: uses the virtual UART, BASE, TIME/WFI and VM-local SRST only.
core::arch::global_asm!(include_str!("guest.S"),
    uart = const pauli_config::GuestBoardLayout::DEFAULT.uart.base,
    time = const pauli_arch_riscv::sbi::TIME,
    base = const pauli_arch_riscv::sbi::BASE,
    srst = const pauli_arch_riscv::sbi::SRST,
    stip = const pauli_arch_riscv::bits::STIP,
    sie = const pauli_arch_riscv::bits::SSTATUS_SIE,
    timer_cause = const pauli_arch_riscv::VmExit::SUPERVISOR_TIMER,
);
pub fn image() -> &'static [u8] {
    unsafe extern "C" {
        static wave8_guest_start: u8;
        static wave8_guest_end: u8;
    }
    let start = &raw const wave8_guest_start;
    // SAFETY: assembler labels delimit the same retained read-only section.
    unsafe {
        core::slice::from_raw_parts(start, &raw const wave8_guest_end as usize - start as usize)
    }
}
pub fn service() -> &'static [u8] {
    unsafe extern "C" {
        static wave8_service_start: u8;
        static wave8_service_end: u8;
    }
    let start = &raw const wave8_service_start;
    // SAFETY: the assembly labels bound the immutable, page-aligned PIC service in host rodata.
    unsafe {
        core::slice::from_raw_parts(
            start,
            (&raw const wave8_service_end as usize) - start as usize,
        )
    }
}
