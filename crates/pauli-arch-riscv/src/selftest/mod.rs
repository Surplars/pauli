//! Opt-in guest assembly fixtures; nothing executes on the ordinary boot path.
//! Ownership/Lock Ordering: caller owns private guest backing and IRQ-masked native context.
//! Invariants: retain code/backing through completion and retire translations before reuse.

pub mod fp;
mod standalone;
pub use standalone::probe;
core::arch::global_asm!(
    include_str!("exception_guest.S"),
    include_str!("wfi_guest.S"),
    include_str!("timer_guest.S"),
    include_str!("paging_guest.S"),
    include_str!("parallel_guest.S"),
    include_str!("fp_guest.S"),
    include_str!("aia/guest.S"),
    include_str!("resident/guest.S"),
);

unsafe extern "C" {
    static pauli_wfi_guest_start: u8;
    static pauli_wfi_guest_end: u8;
}
/// Stackless integer fixture; its first WFI must exit to the native owner.
pub fn integer_image() -> &'static [u8] {
    let start = core::ptr::addr_of!(pauli_wfi_guest_start);
    let end = core::ptr::addr_of!(pauli_wfi_guest_end);
    // SAFETY: assembler symbols bound one immutable retained linker section.
    unsafe { core::slice::from_raw_parts(start, end as usize - start as usize) }
}

mod decoder;
pub mod images;
mod install;
mod words;
pub use decoder::check_mmio_decoder;
pub use install::install_minimal;
