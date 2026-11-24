//! One compile-time instrumentation seam; the default is an empty ZST.
//! Ownership/Lock Ordering: owner-local tracer, no logging or locks on callbacks.
//! Invariants: NoopTracer stores no data; no sampling flags change context layout.
use crate::arch::ExitKind;
pub trait Tracer {
    /// Opt in to a native tick read on each side of a world switch.
    /// Callbacks run on the pinned owner with host IRQs masked and no VM locks.
    const MEASURE_ELAPSED: bool = false;
    fn on_vm_enter(&mut self, _vcpu: u8) {}
    fn on_vm_exit(&mut self, _vcpu: u8, _kind: ExitKind, _elapsed_ticks: u64) {}
    fn on_mmio(&mut self, _gpa: usize, _is_write: bool) {}
}
#[derive(Default, Clone, Copy)]
pub struct NoopTracer;
impl Tracer for NoopTracer {}
