//! Split-ring notification suppression. Observed backlog stays with the service
//! owner; exhausting that backlog arms the next kick, even while I/O is pending.
//! Ownership/Lock Ordering: device owner, then checked guest memory; no locks here.
//! Invariants: publish used.idx before suppression reads; arm then recheck avail.idx.
//! Lifecycle: backlog -> disarmed; consumed -> armed -> recheck -> backlog/armed.
use super::*;

/// Per-epoch protocol counts, not latency or guest CPU-utilization estimates.
#[derive(Clone, Copy, Default)]
pub struct NotificationStats {
    pub kicks: u64,
    pub checks: u64,
    pub suppressed: u64,
    pub rearms: u64,
    pub reuses: u64,
    pub recheck_races: u64,
}

impl<B: AsMut<[u8]>> VirtioBlock<B> {
    /// Whether this epoch selected index-based split-ring notifications.
    pub fn event_idx(&self) -> bool {
        self.driver_features & EVENT_IDX != 0
    }

    /// Called after publishing used.idx. One completed batch may cross the
    /// requested event even if its final index differs from used_event + 1.
    pub(super) fn notify_used(&mut self, memory: &impl GuestMemory, old: u16) -> Option<()> {
        self.notifications.checks += 1;
        // Pair with the driver's notification-enable + used.idx recheck.
        // Suppression must be read AFTER publication, including on weak ISAs.
        fence(Ordering::SeqCst);
        let event_idx = self.event_idx();
        let address = if event_idx {
            self.available + 4 + 2 * u64::from(self.queue_num)
        } else {
            self.available
        };
        let mut raw = [0; 2];
        memory.read(address, &mut raw).then_some(())?;
        let event = u16::from_le_bytes(raw);
        let notify = if event_idx {
            // Wrapping distance tests membership in [old, used_index). Ring
            // bounds keep this interval far shorter than the 16-bit epoch.
            self.used_index.wrapping_sub(event).wrapping_sub(1) < self.used_index.wrapping_sub(old)
        } else {
            event & 1 == 0
        };
        if notify {
            self.irq_edges += u64::from(self.interrupts == 0);
            self.interrupts |= IRQ_USED;
        } else {
            self.notifications.suppressed += 1;
        }
        Some(())
    }

    /// No observed available entries remain. Publish the next index requiring
    /// a kick, then recheck for a driver racing that publication. In-flight I/O
    /// must not defer new admission until its completion or the fallback timer.
    pub(super) fn rearm_available(&mut self, memory: &mut impl GuestMemory) -> Option<()> {
        // No entry consumed since publication: the existing event still asks
        // the guest to kick its next post. Service already read avail.idx; a
        // later post must kick, so busy polls need no new store/fence/recheck.
        if self.available_armed {
            self.notifications.reuses += 1;
            return Some(());
        }
        let address = self.used + 4 + 8 * u64::from(self.queue_num);
        memory.write(self.used, &0u16.to_le_bytes()).then_some(())?;
        memory
            .write(address, &self.last_available.to_le_bytes())
            .then_some(())?;
        fence(Ordering::SeqCst);
        let mut raw = [0; 2];
        memory.read(self.available + 2, &mut raw).then_some(())?;
        let pending = u16::from_le_bytes(raw).wrapping_sub(self.last_available);
        if usize::from(pending) + self.inflight > usize::from(self.queue_num) {
            return None;
        }
        self.notified = pending != 0;
        self.available_armed = true;
        self.notifications.rearms += 1;
        self.notifications.recheck_races += u64::from(self.notified);
        Some(())
    }
}
