//! Same-owner WFI timer hints for one round-robin pass; no locks or live VM references.
//! The mail scan finds future minima and, only for an outstanding offer, another
//! waiter on the same compare. No hint authorizes entry or injects an IRQ.

/// No placement lane can have this index.
pub(super) const NO_LANE: usize = usize::MAX;

/// Normal -> Offered -> Served -> regular rotation. Short handlers for the
/// same compare may relay the offer, but only within its first slice budget.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TimerTurn {
    Normal,
    Offered(Offer),
    Served { lane: usize, epoch: u32 },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Compare {
    pub lane: usize,
    pub at: u64,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Offer {
    pub target: Compare,
    pub until: u64, // MAX on the first admission; subsequent admissions inherit a fixed end.
}
#[derive(Clone, Copy)]
pub(super) enum Visit {
    Mail,
    Regular(Compare),
    Due {
        offer: Offer,
        peer: u64,
        sibling: usize,
    },
}
impl Visit {
    pub fn peer(self) -> Option<u64> {
        match self {
            Self::Mail => None,
            Self::Regular(p) => Some(p.at),
            Self::Due { peer, .. } => Some(peer),
        }
    }
}
const DISARMED: Compare = Compare {
    lane: NO_LANE,
    at: u64::MAX,
};

pub(super) struct ParkedTimers {
    first: Compare,
    second: Compare,
    sampled_at: Option<u64>,
    watch: Compare,
    pub sibling: usize,
}

impl ParkedTimers {
    pub(super) const EMPTY: Self = Self {
        first: DISARMED,
        second: DISARMED,
        sampled_at: None,
        watch: DISARMED,
        sibling: NO_LANE,
    };

    pub fn for_turn(turn: TimerTurn) -> Self {
        match turn {
            TimerTurn::Offered(offer) => Self {
                watch: offer.target,
                ..Self::EMPTY
            },
            _ => Self::EMPTY,
        }
    }

    pub(super) fn observe(&mut self, lane: usize, at: u64, now: impl FnOnce() -> u64) {
        // Disarmed is MAX. When nobody is timed there is no extra clock read.
        if at == u64::MAX {
            return;
        }
        if self.watch.at == at && self.watch.lane != lane && self.sibling == NO_LANE {
            self.sibling = lane;
        }
        if at >= self.second.at || at <= *self.sampled_at.get_or_insert_with(now) {
            return;
        }
        if at < self.first.at {
            self.second = self.first;
            self.first = Compare { lane, at };
        } else {
            self.second = Compare { lane, at };
        }
    }

    pub(super) fn for_peer(&self, lane: usize) -> Compare {
        // A waiter may have become runnable after the scan (e.g. external IRQ).
        // Its old compare must not limit its own slice after it rearms VS TIME.
        if lane == self.first.lane {
            self.second
        } else {
            self.first
        }
    }
}

/// One admission on the owner's monotonic clock. Protection is the unused
/// part of one earlier quantum, never a bonus beyond the normal quantum.
pub(super) struct Grant {
    pub deadline: u64,
    start: u64,
    regular_end: u64,
    protected: u64,
}

impl Grant {
    pub(super) fn new(start: u64, quantum: u64, peer: u64, protected: u64) -> crate::Result<Self> {
        let regular_end = start
            .checked_add(quantum)
            .ok_or(crate::HvError::InvalidState)?;
        debug_assert!(protected <= quantum);
        let deadline = if start < peer && peer < regular_end {
            peer.max(start + protected)
        } else {
            regular_end
        };
        Ok(Self {
            deadline,
            start,
            regular_end,
            protected,
        })
    }

    /// Account admitted wall time (including inline emulation), not inferred
    /// guest instruction time. Zero-entry attempts cannot spend protected time.
    pub(super) fn remaining(&self, entered: bool, now: impl FnOnce() -> u64) -> u64 {
        if self.protected != 0 {
            // A repayment can carry a balance after a native IRQ, but cannot
            // create another balance from this same turn.
            return if entered {
                (self.start + self.protected).saturating_sub(now())
            } else {
                self.protected
            };
        }
        if self.deadline == self.regular_end {
            return 0; // Ordinary slice: no extra TIME read.
        }
        if !entered {
            return self.regular_end - self.start;
        }
        let end = now();
        if end < self.deadline {
            0
        } else {
            self.regular_end.saturating_sub(end)
        }
    }

    /// A distant compare cannot be due within this grant. Do not re-pin that
    /// waiter or read TIME again on the next dispatch just to reject it.
    fn next_turn(&self, peer: Compare) -> TimerTurn {
        if peer.lane != NO_LANE && peer.at <= self.regular_end {
            TimerTurn::Offered(Offer {
                target: peer,
                until: u64::MAX,
            })
        } else {
            TimerTurn::Normal
        }
    }

    /// Only a handler that consumed its compare and went back to sleep can give
    /// remaining time to a sibling. Ordinary rotation reads no extra clock or epoch.
    pub fn after_entry(
        &self,
        visit: Visit,
        waiting: bool,
        next: impl FnOnce() -> Option<u64>,
        now: impl FnOnce() -> u64,
        epoch: impl FnOnce() -> u32,
    ) -> TimerTurn {
        match visit {
            Visit::Regular(peer) => self.next_turn(peer),
            Visit::Due { offer, sibling, .. } => {
                if waiting && sibling != NO_LANE {
                    // Fix the end once. A shorter middle guest has its own cap
                    // but cannot shrink or renew the budget for later guests.
                    let end = if offer.until == u64::MAX {
                        self.regular_end
                    } else {
                        offer.until
                    };
                    let now = now();
                    if now < self.deadline.min(end) && next().is_none_or(|at| at > now) {
                        return TimerTurn::Offered(Offer {
                            target: Compare {
                                lane: sibling,
                                at: offer.target.at,
                            },
                            until: end,
                        });
                    }
                }
                TimerTurn::Served {
                    lane: offer.target.lane,
                    epoch: epoch(),
                }
            }
            Visit::Mail => TimerTurn::Normal, // A mail-only pass never enters.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_regular_turn_only_nominates_a_peer_within_its_own_budget() {
        for (at, offer) in [(200, true), (400, true), (401, false), (u64::MAX, false)] {
            let grant = Grant::new(100, 300, at, 0).unwrap();
            assert_eq!(
                matches!(
                    grant.next_turn(Compare { lane: 7, at }),
                    TimerTurn::Offered(Offer {
                        target: Compare { lane: 7, .. },
                        ..
                    })
                ),
                offer
            );
        }
    }
    #[test]
    fn disarmed_lanes_do_not_read_time_and_equal_peer_deadlines_still_count() {
        let mut p = ParkedTimers::EMPTY;
        p.observe(0, u64::MAX, || panic!("no active timer"));
        p.observe(1, 20, || 20); // Already due: ordinary wake path handles it.
        for (lane, at) in [(2, 50), (3, 40), (4, 40)] {
            p.observe(lane, at, || panic!("sample only once per scan"));
        }
        for lane in 0..5 {
            let peer = p.for_peer(lane);
            assert_eq!(peer.at, 40);
            assert_ne!(peer.lane, lane);
        }
    }

    #[test]
    fn zero_entry_never_spends_credit_or_reads_time() {
        let no_clock = || panic!("no charge without entry");
        for (peer, balance, expected) in [(180, 0, 300), (180, 80, 80), (u64::MAX, 0, 0)] {
            let grant = Grant::new(100, 300, peer, balance).unwrap();
            assert_eq!(grant.remaining(false, no_clock), expected);
        }
        let regular = Grant::new(100, 300, u64::MAX, 0).unwrap();
        assert_eq!(
            regular.remaining(true, || panic!("no extra regular-path TIME read")),
            0
        );
    }

    #[test]
    fn only_time_withheld_by_a_fresh_compare_creates_a_balance() {
        for (returned, expected) in [(179, 0), (180, 220), (200, 200), (400, 0), (450, 0)] {
            let cut = Grant::new(100, 300, 180, 0).unwrap();
            assert_eq!(cut.remaining(true, || returned), expected);
        }
        assert!(Grant::new(u64::MAX - 1, 300, u64::MAX, 0).is_err());
    }

    #[test]
    fn protection_is_a_floor_not_a_second_cut_or_a_ceiling() {
        for (peer, end) in [(100, 400), (150, 240), (350, 350), (u64::MAX, 400)] {
            let grant = Grant::new(100, 300, peer, 140).unwrap();
            assert_eq!(grant.deadline, end);
            assert_eq!(grant.remaining(true, || end), 0);
        }
        let interrupted = Grant::new(100, 300, 150, 140).unwrap();
        assert_eq!(interrupted.remaining(true, || 200), 40);
    }
}
