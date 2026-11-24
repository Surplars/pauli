//! Host-only HAL tests with private memory and fake per-thread IRQ state.
//! Ownership/Lock Ordering: fixtures are local; threads share only explicit test locks.
//! Invariants: no actual MMIO, DMA, firmware or CPU interrupt instruction executes.

mod fdt;
mod memory;
mod synchronization;

use core::cell::Cell;
std::thread_local! { static IRQ_ENABLED: Cell<bool> = const { Cell::new(true) }; }
pub(super) struct FakeIrq;
// SAFETY: tests have no hardware interrupts; TLS models same-thread mask/restore nesting.
unsafe impl crate::sync::LocalIrq for FakeIrq {
    type Saved = bool;
    fn save() -> bool {
        IRQ_ENABLED.with(|flag| flag.replace(false))
    }
    unsafe fn restore(saved: bool) {
        IRQ_ENABLED.with(|flag| flag.set(saved));
    }
}
