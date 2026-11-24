//! Native SBI HSM launch records; guest HSM requests never call this interface.
//! Ownership/Lock Ordering: boot owner release-publishes; worker acquires before reading its stack.
//! Invariants: stacks/records remain pinned; tp is the explicit physical CPU index.

use super::sbi::{self, SbiRet};
use core::sync::atomic::{AtomicUsize, Ordering};
#[repr(C)]
pub struct HartBoot {
    stack_top: usize,
    entry: extern "C" fn(usize, usize) -> !,
    opaque: usize,
    published: AtomicUsize,
    cpu: usize,
}
impl HartBoot {
    /// # Safety
    /// Private identity-mapped 16-byte-aligned native stack and retained entry/
    /// opaque data. None of the backing may be assigned to a guest.
    pub unsafe fn new(
        stack_top: usize,
        entry: extern "C" fn(usize, usize) -> !,
        opaque: usize,
        cpu: usize,
    ) -> Result<Self, crate::Error> {
        if stack_top & 15 != 0 || cpu >= crate::MAX_CPUS {
            return Err(crate::Error::InvalidState);
        }
        Ok(Self {
            stack_top,
            entry,
            opaque,
            published: AtomicUsize::new(0),
            cpu,
        })
    }
    /// # Safety
    /// The stopped native hart is exclusively assigned; retain self/stack and
    /// all entry-reachable data until explicit worker join and SBI stopped state.
    pub unsafe fn start(&'static self, hart: usize) -> SbiRet {
        unsafe extern "C" {
            fn pauli_host_entry();
        }
        self.published.store(1, Ordering::Release);
        // SAFETY: caller owns the stopped hart and retains the identity-mapped launch record.
        unsafe {
            sbi::hart_start(
                hart,
                pauli_host_entry as *const () as usize,
                self as *const _ as usize,
            )
        }
    }
}
core::arch::global_asm!(
    ".section .text", ".balign 4", ".global pauli_host_entry", "pauli_host_entry:",
    ".option push", ".option norelax", "la gp, __global_pointer$", ".option pop",
    "1:", "ld t0, {published}(a1)", "beqz t0, 1b", "fence r, rw",
    "ld tp, {cpu}(a1)", "ld sp, {stack}(a1)", "ld t0, {entry}(a1)", "ld a1, {opaque}(a1)",
    "fence.i", "jr t0",
    published = const core::mem::offset_of!(HartBoot, published), cpu = const core::mem::offset_of!(HartBoot, cpu),
    stack = const core::mem::offset_of!(HartBoot, stack_top), entry = const core::mem::offset_of!(HartBoot, entry), opaque = const core::mem::offset_of!(HartBoot, opaque),
);
