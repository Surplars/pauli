//! Selftest-only sampling of the native wait, never part of the production idle path.
//! Each owner writes only its own counter; the monitor reads it without locking.
use super::*;
use core::sync::atomic::AtomicU64;

pub(super) struct IdleCounters {
    pending: [AtomicU64; 5],
    guest_late: [AtomicU64; 4],
    // Low bit brackets a disarmed WFI; upper bits count native SSIP-only returns.
    workers: [AtomicU64; arch::MAX_CPUS],
}
impl Default for IdleCounters {
    fn default() -> Self {
        Self {
            pending: Default::default(),
            guest_late: Default::default(),
            workers: [const { AtomicU64::new(0) }; arch::MAX_CPUS],
        }
    }
}
impl IdleCounters {
    fn record<T: ArchTimer>(&self, kind: arch::IdleWake, guest_compare: Option<u64>, hz: u64) {
        // One native writer; the monitor reads between dispatches on the same hart.
        let bump = |n: &core::sync::atomic::AtomicU64| {
            n.store(n.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
        };
        // Sample before updating counters. Early or mixed returns are not evidence
        // of lateness against a selected guest compare; never ascribe them to it.
        if kind == arch::IdleWake::Timer
            && let Some(at) = guest_compare
            && let Some(late) = T::now().checked_sub(at)
        {
            // Ceil boundaries keep the bins disjoint for non-integer ticks per ms.
            let bin = if late < hz.div_ceil(1_000) {
                0
            } else if late < hz.div_ceil(200) {
                1
            } else if late < hz.div_ceil(100) {
                2
            } else {
                3
            };
            bump(&self.guest_late[bin]);
        }
        bump(&self.pending[kind as usize]);
    }
}

impl<A: HvArch, B: Board> VmRuntime<A, B> {
    pub(crate) fn idle_wakes(&self) -> Option<([u64; 5], [u64; 4])> {
        (self.board.info().host_cpus == 1 && A::Smp::SAMPLE_IDLE.is_some()).then(|| {
            let read = |n: &core::sync::atomic::AtomicU64| n.load(Ordering::Relaxed);
            (
                self.idle_wakes.pending.each_ref().map(read),
                self.idle_wakes.guest_late.each_ref().map(read),
            )
        })
    }

    pub(crate) fn worker_wait(&self, owner: usize) -> Option<(bool, u64)> {
        let info = self.board.info();
        (self.firmware_only && owner != info.boot_cpu && A::Smp::SAMPLE_IDLE.is_some()).then(|| {
            let word = self.idle_wakes.workers[owner].load(Ordering::Acquire);
            (word & 1 != 0, word >> 1)
        })
    }

    /// # Safety
    /// The dispatcher owns this hart, with IRQs masked, banks put and wake sources armed.
    pub(super) unsafe fn observe_idle(
        &self,
        owner: usize,
        disarmed: bool,
        guest: Option<u64>,
        interval: Option<&mut arch::TickInterval>,
    ) {
        let sample = A::Smp::SAMPLE_IDLE;
        let bracket = (disarmed && self.firmware_only && sample.is_some()).then(|| {
            let counter = &self.idle_wakes.workers[owner];
            let base = counter.load(Ordering::Relaxed);
            counter.store(base | 1, Ordering::Release);
            (counter, base)
        });
        // SAFETY: the caller established the ordinary dispatcher's WFI preconditions.
        arch::TickInterval::measure(interval, A::Timer::now, || unsafe { A::Smp::idle() });
        if let Some(sample) = sample {
            if let Some((counter, base)) = bracket {
                let software = sample() == arch::IdleWake::Software;
                counter.store(
                    base.wrapping_add(u64::from(software) << 1),
                    Ordering::Release,
                );
            } else if self.board.info().host_cpus == 1 {
                self.idle_wakes
                    .record::<A::Timer>(sample(), guest, self.board.info().timebase_hz);
            }
        }
    }
}
