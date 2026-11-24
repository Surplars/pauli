//! Opt-in architecture acceptance fixtures, absent from ordinary boot.
//! Ownership/Lock Ordering: caller owns inactive private guest backing and masked IRQs.
//! Invariants: fixture ranges do not overlap; every load/copy retains private RAM.
use super::images::*;
/// Install stackless RV64 IMAC and optional F/D fixtures into exclusively assigned
/// identity-mapped storage. The caller assigns GPA offsets to a VM, not ISA.
/// # Safety
/// `page` must be a private writable 4096-byte page, not currently executing.
pub unsafe fn install_minimal(page: *mut u8) {
    // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
    unsafe { core::ptr::write_bytes(page, 0, 4096) };
    for &(offset, words) in super::words::WORDS {
        for (index, word) in words.iter().enumerate() {
            // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
            unsafe { page.add(offset + index * 4).cast::<u32>().write(*word) };
        }
    }
    let start = core::ptr::addr_of!(pauli_timer_guest_start);
    let size = core::ptr::addr_of!(pauli_timer_guest_end) as usize - start as usize;
    assert!(TIMER_OFFSET + size <= 0x400); // do not overlap replacement fixture
    // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
    unsafe { core::ptr::copy_nonoverlapping(start, page.add(TIMER_OFFSET), size) };
    let start = core::ptr::addr_of!(pauli_wfi_guest_start);
    let size = core::ptr::addr_of!(pauli_wfi_guest_end) as usize - start as usize;
    assert!(WFI_OFFSET + size <= PAGING_OFFSET);
    // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
    unsafe { core::ptr::copy_nonoverlapping(start, page.add(WFI_OFFSET), size) };
    let start = core::ptr::addr_of!(pauli_paging_guest_start);
    let size = core::ptr::addr_of!(pauli_paging_guest_end) as usize - start as usize;
    assert!(PAGING_OFFSET + size <= BENCH_OFFSET);
    // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
    unsafe { core::ptr::copy_nonoverlapping(start, page.add(PAGING_OFFSET), size) };
    let start = core::ptr::addr_of!(pauli_exception_guest_start);
    let size = core::ptr::addr_of!(pauli_exception_guest_end) as usize - start as usize;
    assert!(EXCEPTION_OFFSET + size <= FP_OFFSET);
    // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
    unsafe { core::ptr::copy_nonoverlapping(start, page.add(EXCEPTION_OFFSET), size) };
    let start = core::ptr::addr_of!(pauli_fp_guest_start);
    let size = core::ptr::addr_of!(pauli_fp_guest_end) as usize - start as usize;
    assert!(FP_OFFSET + size <= 4096);
    // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
    unsafe { core::ptr::copy_nonoverlapping(start, page.add(FP_OFFSET), size) };
    // Publish newly written instruction bytes before executing through G-stage.
    // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
    unsafe { core::arch::asm!("fence.i", options(nostack)) };
}
