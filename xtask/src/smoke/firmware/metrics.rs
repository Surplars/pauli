//! Numeric evidence from the two guest consoles and optional native idle diagnostics.
//! Ownership: immutable, already source/epoch-qualified lines; no device access or locks.
//! Invariants: never attribute a partial, skipped or regressed release window to a phase.
use anyhow::{Context, Result, ensure};
use std::fmt;

pub(super) const WINDOW: u64 = 32;

#[derive(Clone, Copy, Default)]
pub(super) struct Boundary {
    pub samples: u64,
    pub missed: u64,
}
impl Boundary {
    pub fn parse(line: &str) -> Result<Self> {
        let (samples, missed) = line.split_once(" missed=").context("invalid RTOS anchor")?;
        Self::new(samples, missed)
    }
    fn new(count: &str, missed: &str) -> Result<Self> {
        let samples: u64 = count.parse()?;
        ensure!(
            samples > 0 && samples.is_multiple_of(WINDOW),
            "invalid release boundary"
        );
        Ok(Self {
            samples,
            missed: missed.parse()?,
        })
    }
}
#[derive(Default)]
pub(super) struct Score {
    pub last: Boundary,
    pub releases: u128,
    pub missed: u128,
}
impl Score {
    /// Only add windows after their reports and peer work were verified.
    /// Releases and misses during the handshakes between rounds do not count.
    pub fn add(&mut self, anchor: Boundary, end: Boundary) -> Result<()> {
        ensure!(
            anchor.samples >= self.last.samples && anchor.missed >= self.last.missed,
            "scored windows overlap or missed counter regressed between rounds"
        );
        self.releases += u128::from(end.samples - anchor.samples);
        self.missed += u128::from(end.missed - anchor.missed);
        self.last = end;
        Ok(())
    }
}
pub(super) struct Report {
    pub end: Boundary,
    pub peak: u64,
}
impl Report {
    pub fn parse(line: &str) -> Result<Self> {
        let (samples, rest) = line
            .split_once(" max_late_ticks=")
            .context("missing lifetime peak")?;
        let (worst, rest) = rest
            .split_once(" missed=")
            .context("missing missed periods")?;
        let (missed, peak) = rest
            .split_once(" window_max_ticks=")
            .context("missing window peak; rebuild vs-control")?;
        let peak = peak.parse()?;
        ensure!(
            peak <= worst.parse::<u64>()?,
            "window exceeds lifetime peak"
        );
        Ok(Self {
            end: Boundary::new(samples, missed)?,
            peak,
        })
    }
    pub fn check_next(&self, old: Boundary) -> Result<()> {
        ensure!(
            old.samples.checked_add(WINDOW) == Some(self.end.samples)
                && self.end.missed >= old.missed,
            "nonconsecutive RTOS windows or missed counter regressed"
        );
        Ok(())
    }
}
pub(super) fn work(value: &str) -> Result<u64> {
    ensure!(
        value.len() == 16
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid bare work counter"
    );
    Ok(u64::from_str_radix(value, 16)?)
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub(super) struct Wakes([u64; 10]);
// Monitor wire order: five disjoint pending classes, elapsed clock, then a
// histogram for the expired guest compares within the timer-only class.
const NAMES: [&str; 10] = [
    "timer",
    "software",
    "external",
    "multiple",
    "none",
    "ticks",
    "guest_lt1ms",
    "guest_1to5ms",
    "guest_5to10ms",
    "guest_ge10ms",
];
impl Wakes {
    /// Absolute native TIME in a monitor snapshot (delta snapshots hold elapsed ticks).
    pub(super) fn ticks(&self) -> u64 {
        self.0[5]
    }
    pub fn parse(line: &str) -> Result<Self> {
        let mut fields = line.split_ascii_whitespace();
        let mut result = Self::default();
        for (out, name) in result.0.iter_mut().zip(NAMES) {
            let (key, value) = fields
                .next()
                .and_then(|s| s.split_once('='))
                .context("missing WFI count")?;
            ensure!(key == name, "unexpected WFI field {key}");
            *out = value.parse()?;
        }
        ensure!(fields.next().is_none(), "unexpected WFI fields");
        Ok(result)
    }
    pub fn delta(mut self, start: Self) -> Result<Self> {
        for (n, old) in self.0.iter_mut().zip(start.0) {
            *n = n.checked_sub(old).context("WFI snapshot regressed")?;
        }
        ensure!(self.0[5] != 0, "empty native sampling interval");
        ensure!(
            self.binned() <= u128::from(self.0[0]),
            "expired guest timer histogram exceeds timer-only returns"
        );
        Ok(self)
    }
    fn binned(&self) -> u128 {
        self.0[6..].iter().copied().map(u128::from).sum()
    }
    /// Use the validated nonempty native interval, not the number of RTOS releases.
    pub fn normalized(&self, start_ticks: u64, hz: u32) -> String {
        let interval = u128::from(self.ticks());
        let hz = u128::from(hz);
        let tenths_per_s = (u128::from(self.0[0]) * hz * 10 + interval / 2) / interval;
        let late = match self.binned() {
            0 => "n/a".to_string(),
            n => format!("{}/{n}", self.0[9]),
        };
        format!(
            "start_ms={} timer_only_returns_per_s={}.{} guest_ge10ms_over_binned={late}",
            u128::from(start_ticks) * 1000 / hz,
            tenths_per_s / 10,
            tenths_per_s % 10,
        )
    }
}
impl fmt::Display for Wakes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [t, s, e, m, n, ticks, a, b, c, d] = self.0;
        write!(
            f,
            "timer={t} software={s} external={e} multiple={m} none={n} interval_ticks={ticks}; \
             expired guest timer lateness counts: guest_lt1ms={a} guest_1to5ms={b} \
             guest_5to10ms={c} guest_ge10ms={d}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_causally_scored_windows_contribute_to_totals() -> Result<()> {
        let at = |samples, missed| Boundary { samples, missed };
        let mut s = Score::default();
        // Four missed periods before the first round and another in a gap.
        s.add(at(32, 4), at(96, 5))?;
        s.add(at(128, 6), at(192, 6))?;
        assert_eq!((s.releases, s.missed), (128, 1));
        assert!(s.add(at(160, 6), at(224, 6)).is_err());
        assert!(s.add(at(224, 5), at(288, 5)).is_err());
        assert_eq!((s.releases, s.missed), (128, 1));
        Ok(())
    }
    #[test]
    fn normalization_uses_timer_only_rate_and_guest_binned_denominator() {
        let delta = Wakes([117, 0, 24, 1, 0, 18_071_145, 0, 1, 0, 115]);
        assert_eq!(
            delta.normalized(60_073_187, 10_000_000),
            "start_ms=6007 timer_only_returns_per_s=64.7 guest_ge10ms_over_binned=115/116"
        );
        let extreme = Wakes([u64::MAX, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
        assert_eq!(
            extreme.normalized(u64::MAX, u32::MAX),
            format!(
                "start_ms={} timer_only_returns_per_s={}.0 guest_ge10ms_over_binned=n/a",
                u128::from(u64::MAX) * 1000 / u128::from(u32::MAX),
                u128::from(u64::MAX) * u128::from(u32::MAX)
            )
        );
    }
    #[test]
    fn window_max_is_not_the_lifetime_peak() {
        let anchor = Boundary::parse("32 missed=9").unwrap();
        let report = Report::parse("64 max_late_ticks=90 missed=12 window_max_ticks=5").unwrap();
        assert!(report.check_next(anchor).is_ok());
        assert_eq!((report.peak, report.end.missed - anchor.missed), (5, 3));
        for s in ["32 missed=13", "64 missed=12", "96 missed=12"] {
            assert!(report.check_next(Boundary::parse(s).unwrap()).is_err());
        }
        for s in [
            "48 max_late_ticks=3 missed=0 window_max_ticks=1",
            "64 max_late_ticks=3 missed=0",
            "64 max_late_ticks=0 missed=0 window_max_ticks=1",
        ] {
            assert!(Report::parse(s).is_err());
        }
        assert!(Boundary::parse("0 missed=0").is_err());
    }
    #[test]
    fn busy_work_requires_a_full_uint64() {
        assert_eq!(work("00000000ffffffff").unwrap(), 0xffff_ffff);
        for bad in ["+000000000000001", "1", "0000000000000NaN"] {
            assert!(work(bad).is_err());
        }
    }
    #[test]
    fn wfi_deltas_preserve_units_and_filter_histogram_to_this_round() {
        let a = Wakes::parse(
            "timer=50 software=1 external=7 multiple=1 none=8 ticks=200 \
            guest_lt1ms=4 guest_1to5ms=3 guest_5to10ms=2 guest_ge10ms=1",
        )
        .unwrap();
        let b = Wakes::parse(
            "timer=70 software=2 external=7 multiple=1 none=15 ticks=900 \
            guest_lt1ms=11 guest_1to5ms=8 guest_5to10ms=5 guest_ge10ms=2",
        )
        .unwrap();
        let actual = b.delta(a).unwrap();
        assert_eq!(actual.0, [20, 1, 0, 0, 7, 700, 7, 5, 3, 1]);
        assert!(
            actual
                .to_string()
                .contains("interval_ticks=700; expired guest timer lateness counts: guest_lt1ms=7")
        );
        assert!(b.delta(b).is_err());
        assert!(a.delta(b).is_err());
        assert!(Wakes::parse("timer=2 software=0 external=1 multiple=0 none=0 ticks=700").is_err());
    }
    #[test]
    fn disjoint_guest_bins_cannot_exceed_timer_only_returns_or_wrap() {
        let before = Wakes([100, 0, 0, 0, 0, 1, 2, 0, 0, 0]);
        // Both snapshots in isolation look plausible, but the round gained more bins than timers.
        assert!(
            Wakes([101, 0, 0, 0, 0, 2, 3, 1, 0, 0])
                .delta(before)
                .is_err()
        );
        assert!(
            Wakes([101, 0, 0, 0, 0, 2, 1, 0, 0, 0])
                .delta(before)
                .is_err()
        );
        let max = u64::MAX;
        assert!(
            Wakes([max, 0, 0, 0, 0, 1, max, 1, 0, 0])
                .delta(Wakes::default())
                .is_err()
        );
    }
}
