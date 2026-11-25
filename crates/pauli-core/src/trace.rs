//! Compile-time instrumentation on the pinned owner, outside VM locks.
use crate::arch::{ExitKind, ExitSource, TickInterval, VcpuExit};
use core::sync::atomic::Ordering;
mod stats;
pub use stats::{
    HS_STATS_MAGIC, HS_STATS_VERSION, HsStats, OwnerStats, PAULI_HS_STATS, UNREGISTERED_HART,
    init_host, register_owner,
};
#[cfg(test)]
mod tests;

pub trait Tracer {
    /// Opt in to a native tick read on each side of a world switch.
    const MEASURE_ELAPSED: bool = false;
    /// Request decoded exit classification independently of native tick reads.
    const TRACE_EXITS: bool = false;
    fn on_slice_start(&mut self, _vcpu: u8) {}
    fn on_vm_enter(&mut self, _vcpu: u8) {}
    fn on_vm_exit(&mut self, _vcpu: u8, _kind: ExitKind, _elapsed_ticks: u64) {}
    fn on_guest_interval(&mut self, _vcpu: u8, _source: ExitSource, _interval: TickInterval) {}
    fn on_guest_exit(&mut self, _source: ExitSource, _exit: &VcpuExit, _vtimer_irq: bool) {}
    fn on_vtimer_irq(&mut self) {}
    fn on_idle_end(&mut self, _now: u64, _elapsed_ticks: u64) {}
    fn on_idle_wfi(&mut self) {}
    fn on_mail(&mut self, _send: bool) {}
    fn on_mmio(&mut self, _gpa: usize, _is_write: bool) {}
}

#[derive(Default, Clone, Copy)]
pub struct NoopTracer;
impl Tracer for NoopTracer {}

/// Each dispatcher keeps the address of its own 128-byte slot throughout execution.
pub struct StatsTracer<'a> {
    slot: &'a OwnerStats,
}
impl StatsTracer<'static> {
    pub fn registered_owner(owner: usize) -> Option<Self> {
        Self::with_stats(&PAULI_HS_STATS, owner)
    }
}
impl<'a> StatsTracer<'a> {
    /// Check registration once, before dispatch. Callbacks only touch this slot.
    pub fn with_stats(stats: &'a HsStats, owner: usize) -> Option<Self> {
        let slot = stats.owners.get(owner)?;
        (slot.hart_id.load(Ordering::Acquire) != UNREGISTERED_HART).then_some(Self { slot })
    }
}
impl Tracer for StatsTracer<'_> {
    const TRACE_EXITS: bool = true;

    fn on_guest_exit(&mut self, source: ExitSource, exit: &VcpuExit, vtimer_irq: bool) {
        let slot = self.slot;
        slot.last_scause.store(exit.cause as u64, Ordering::Relaxed);
        slot.last_sepc.store(exit.pc as u64, Ordering::Relaxed);
        slot.last_stval.store(exit.value as u64, Ordering::Relaxed);
        let bucket = match source {
            ExitSource::Wfi => &slot.wfi_exits,
            ExitSource::SbiTimer => &slot.sbi_timer_exits,
            ExitSource::SstcTimer => &slot.sstc_timer_exits,
            ExitSource::Mmio => &slot.mmio_exits,
            ExitSource::External => &slot.ext_irqs,
            ExitSource::Timer
            | ExitSource::Software
            | ExitSource::SupervisorCall
            | ExitSource::Other => &slot.other_exits,
        };
        bucket.fetch_add(1, Ordering::Relaxed);
        if vtimer_irq {
            slot.vtimer_irqs.fetch_add(1, Ordering::Relaxed);
        }
        slot.vm_exits.fetch_add(1, Ordering::Relaxed);
    }

    fn on_idle_wfi(&mut self) {
        self.slot.idle_wfi.fetch_add(1, Ordering::Relaxed);
    }
    fn on_vtimer_irq(&mut self) {
        self.slot.vtimer_irqs.fetch_add(1, Ordering::Relaxed);
    }
    fn on_mail(&mut self, send: bool) {
        let counter = if send {
            &self.slot.mail_sends
        } else {
            &self.slot.mail_recvs
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}
