//! Completion-latch eligibility and bounded fallback polling; no hardware reads.
//! Ownership/Lock Ordering: VM device owner mutates this scheduling state.
//! Invariants: consume event before work, never during rearm; deadlines never renew.
pub struct Service {
    immediate: bool,
    retry_at: Option<u64>,
    interval: u64,
}
impl Service {
    pub fn new(interval: u64) -> Self {
        Self {
            immediate: false,
            retry_at: None,
            interval,
        }
    }
    pub fn kick(&mut self) {
        self.immediate = true;
    }
    pub fn idle(&mut self) {
        self.immediate = false;
        self.retry_at = None;
    }
    pub fn due(&mut self, now: u64, event: bool) -> bool {
        if !event && !self.immediate && self.retry_at.is_none_or(|deadline| now < deadline) {
            return false;
        }
        self.immediate = false;
        self.retry_at = None;
        true
    }
    pub fn rearm(&mut self, now: u64, progress: bool, deadline: Option<u64>) {
        self.immediate = progress;
        let fallback = now.saturating_add(self.interval);
        self.retry_at = Some(deadline.map_or(fallback, |at| at.min(fallback)));
    }
}
