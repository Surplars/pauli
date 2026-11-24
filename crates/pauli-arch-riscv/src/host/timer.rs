//! Native TIME counter/SBI endpoint, with no RTOS tick or scheduling policy.
//! Ownership/Lock Ordering: the physical dispatcher is the sole compare writer.
//! Invariants: guest TIME never invokes this endpoint directly; absolute deadlines persist.

pub fn read_time() -> u64 {
    let now;
    // SAFETY: the board's RV64 timebase contract includes an accessible TIME counter.
    unsafe {
        core::arch::asm!("rdtime {}", out(reg) now, options(nomem, nostack));
    }
    now
}
pub fn set_sbi_timer(deadline: u64) -> super::sbi::SbiRet {
    super::sbi::try_set_timer(deadline)
}
