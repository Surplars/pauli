//! Compile-time instrumentation seam and lightweight JTAG-readable HS stats.
//! Ownership/Lock Ordering: owner-local tracer, no logging or locks on callbacks.
//! Invariants: NoopTracer stores no data; no sampling flags change context layout.
use crate::arch::{ExitKind, ExitSource, TickInterval};
use core::sync::atomic::{AtomicU64, Ordering};

pub trait Tracer {
    /// Opt in to a native tick read on each side of a world switch.
    /// Callbacks run on the pinned owner with host IRQs masked and no VM locks.
    const MEASURE_ELAPSED: bool = false;
    fn on_slice_start(&mut self, _vcpu: u8) {}
    fn on_vm_enter(&mut self, _vcpu: u8) {}
    fn on_vm_exit(&mut self, _vcpu: u8, _kind: ExitKind, _elapsed_ticks: u64) {}
    fn on_guest_interval(&mut self, _vcpu: u8, _source: ExitSource, _interval: TickInterval) {}
    fn on_idle_end(&mut self, _now: u64, _elapsed_ticks: u64) {}
    fn on_mmio(&mut self, _gpa: usize, _is_write: bool) {}
}

#[derive(Default, Clone, Copy)]
pub struct NoopTracer;
impl Tracer for NoopTracer {}

pub const HS_STATS_MAGIC: u64 = 0x5041_554c_4953_5431;
pub const HS_STATS_VERSION: u64 = 1;

/// Boot-hart counters, starting at the first measured switch or WFI. Guest time
/// includes the assembly switch envelope. HS time covers the gaps between
/// measured intervals. Individual words are atomic. For consistent cross-field
/// totals, halt the hart between trace callbacks.
#[repr(C)]
pub struct HsStats {
    pub magic: u64,
    pub version: u64,
    pub entries: AtomicU64,
    pub guest_ticks: AtomicU64,
    pub hs_ticks: AtomicU64,
    pub idle_ticks: AtomicU64,
    pub slices: AtomicU64,
    pub exit_timer: AtomicU64,
    pub exit_external: AtomicU64,
    pub exit_software: AtomicU64,
    pub exit_sbi: AtomicU64,
    pub exit_mmio: AtomicU64,
    pub exit_wfi: AtomicU64,
    pub exit_other: AtomicU64,
    pub last_tick: AtomicU64,
    pub sstc_active: AtomicU64,
}

const _: () = {
    assert!(core::mem::size_of::<HsStats>() == 16 * 8);
    assert!(core::mem::offset_of!(HsStats, last_tick) == 14 * 8);
};

impl HsStats {
    pub const fn new() -> Self {
        Self {
            magic: HS_STATS_MAGIC,
            version: HS_STATS_VERSION,
            entries: AtomicU64::new(0),
            guest_ticks: AtomicU64::new(0),
            hs_ticks: AtomicU64::new(0),
            idle_ticks: AtomicU64::new(0),
            slices: AtomicU64::new(0),
            exit_timer: AtomicU64::new(0),
            exit_external: AtomicU64::new(0),
            exit_software: AtomicU64::new(0),
            exit_sbi: AtomicU64::new(0),
            exit_mmio: AtomicU64::new(0),
            exit_wfi: AtomicU64::new(0),
            exit_other: AtomicU64::new(0),
            last_tick: AtomicU64::new(0),
            sstc_active: AtomicU64::new(0),
        }
    }
}

#[unsafe(no_mangle)]
pub static PAULI_HS_STATS: HsStats = HsStats::new();

pub fn set_sstc_active(active: bool) {
    PAULI_HS_STATS
        .sstc_active
        .store(u64::from(active), Ordering::Relaxed);
}

pub struct StatsTracer {
    stats: &'static HsStats,
    last_end: Option<u64>,
}

impl StatsTracer {
    pub const fn global() -> Self {
        Self::with_stats(&PAULI_HS_STATS)
    }

    pub const fn with_stats(stats: &'static HsStats) -> Self {
        Self {
            stats,
            last_end: None,
        }
    }

    fn account_hs_until(&mut self, start: u64) {
        if let Some(prev) = self.last_end {
            self.stats
                .hs_ticks
                .fetch_add(start.wrapping_sub(prev), Ordering::Relaxed);
        }
    }
}

impl Tracer for StatsTracer {
    const MEASURE_ELAPSED: bool = true;

    fn on_slice_start(&mut self, _vcpu: u8) {
        self.stats.slices.fetch_add(1, Ordering::Relaxed);
    }

    fn on_guest_interval(&mut self, _vcpu: u8, source: ExitSource, interval: TickInterval) {
        self.account_hs_until(interval.start);
        self.stats.entries.fetch_add(1, Ordering::Relaxed);
        self.stats
            .guest_ticks
            .fetch_add(interval.elapsed(), Ordering::Relaxed);
        self.last_end = Some(interval.end);
        self.stats.last_tick.store(interval.end, Ordering::Relaxed);
        let bucket = match source {
            ExitSource::Timer => &self.stats.exit_timer,
            ExitSource::External => &self.stats.exit_external,
            ExitSource::Software => &self.stats.exit_software,
            ExitSource::SupervisorCall => &self.stats.exit_sbi,
            ExitSource::Mmio => &self.stats.exit_mmio,
            ExitSource::Wfi => &self.stats.exit_wfi,
            ExitSource::Other => &self.stats.exit_other,
        };
        bucket.fetch_add(1, Ordering::Relaxed);
    }

    fn on_idle_end(&mut self, now: u64, elapsed_ticks: u64) {
        self.account_hs_until(now.wrapping_sub(elapsed_ticks));
        self.stats
            .idle_ticks
            .fetch_add(elapsed_ticks, Ordering::Relaxed);
        self.last_end = Some(now);
        self.stats.last_tick.store(now, Ordering::Relaxed);
    }
}
