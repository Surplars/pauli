//! Host-side reporting for causal UART IRQ samples, outside the measured interval.
//! Ownership: the serial controller supplies one complete timestamp triple per guest sample.
//! Invariants: subtract within a single sample; independent percentiles do not add.
use anyhow::{Context, Result, ensure};
use std::io::{self, Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AckPath {
    GuestExit,
    DispatchAck,
}
impl AckPath {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::GuestExit => "guest-exit",
            Self::DispatchAck => "dispatch-ack",
        }
    }
}

/// A positive acknowledgement of the shared native SSIP in this send window.
/// It is not a per-VM interrupt source tag or the physical arrival time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Ack {
    pub(super) path: AckPath,
    pub(super) send: u64,
    pub(super) ticks: u64,
    /// Sender-local, independent of ACK, owner observation and guest trap.
    pub(super) returned: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Sample {
    input: u64,
    seq: u64,
    rise: u64,
    to_owner: u64,
    to_trap: u64,
    pub(super) ack: Option<Ack>,
}

impl Sample {
    pub(super) fn new(input: u64, seq: u64, rise: u64, owner: u64, trap: u64) -> Result<Self> {
        Ok(Self {
            input,
            seq,
            rise,
            to_owner: owner
                .checked_sub(rise)
                .context("owner observation predates UART rise")?,
            to_trap: trap
                .checked_sub(owner)
                .context("VS trap predates owner observation")?,
            ack: None,
        })
    }
    pub(super) fn with_ack(mut self, ack: Option<Ack>) -> Result<Self> {
        if let Some(a) = ack {
            ensure!(
                a.send <= a.returned,
                "native kick returned before it was sent"
            );
            ensure!(
                self.rise <= a.send && a.send < a.ticks && a.ticks <= self.rise + self.to_owner,
                "shared SSIP observation outside the sample's send/owner interval"
            );
        }
        self.ack = ack;
        Ok(self)
    }
    fn total(&self) -> u64 {
        self.to_owner + self.to_trap // Exactly trap - rise; ordering was checked above.
    }
    pub(super) fn raw(self) -> [u64; 5] {
        [
            self.input,
            self.seq,
            self.rise,
            self.rise + self.to_owner,
            self.rise + self.total(),
        ]
    }
}

/// Nearest-rank P50 and observed maximum; the scored ABBA phases are nonempty.
fn distribution(samples: &[Sample], value: impl Fn(&Sample) -> u64) -> (u64, u64) {
    let mut ticks: Vec<_> = samples.iter().map(value).collect();
    ticks.sort_unstable();
    (ticks[(ticks.len() - 1) / 2], ticks[ticks.len() - 1])
}

pub(super) fn report(samples: &[Sample], epoch: u32, owner: u8, mode: &str, hz: u64) -> Result<()> {
    write_report(&mut io::stdout().lock(), samples, epoch, owner, mode, hz)
}

fn write_report(
    out: &mut impl Write,
    samples: &[Sample],
    epoch: u32,
    owner: u8,
    mode: &str,
    hz: u64,
) -> Result<()> {
    let mut ordered = samples.to_vec();
    // Rank by end-to-end latency. Guest sequence breaks ties reproducibly;
    // equal totals can have very different owner and VS contributions.
    ordered.sort_unstable_by_key(|s| (s.total(), s.seq));
    let n = ordered.len();
    let max = ordered.last().context("no UART latency samples")?.total();
    let median = n.div_ceil(2);
    let p50 = ordered[median - 1].total();
    writeln!(
        out,
        "firmware UART rise->VS trap: epoch={epoch} owner={owner} mode={mode} n={n} p50_ticks={p50} max_ticks={max} timebase_hz={hz}"
    )?;
    let (p50_a, max_a) = distribution(samples, |s| s.to_owner);
    let (p50_b, max_b) = distribution(samples, |s| s.to_trap);
    writeln!(
        out,
        "  rise->owner p50_ticks={p50_a} max_ticks={max_a}; owner->VS trap p50_ticks={p50_b} max_ticks={max_b}"
    )?;
    // With eight samples P90 is already the maximum; P75 still gives a distinct rank.
    for (label, rank) in [("p50", median), ("p75", n - n / 4), ("slowest", n)] {
        let s = &ordered[rank - 1];
        let total = s.total();
        let tied = ordered
            .iter()
            .filter(|other| other.total() == total)
            .count();
        writeln!(
            out,
            "  {label} paired: input={} sample={} rise_to_owner_ticks={} owner_to_trap_ticks={} total_ticks={total} rank={rank}/{n} equal_totals={tied}",
            s.input, s.seq, s.to_owner, s.to_trap,
        )?;
    }
    report_acks(out, &ordered)?;
    Ok(())
}

/// Retain all observations for the main ranks. Within each matched path show
/// one *actual* median-total event instead of adding independent stage medians.
fn report_acks(out: &mut impl Write, ordered: &[Sample]) -> io::Result<()> {
    let n = ordered.len();
    let paths = [AckPath::GuestExit, AckPath::DispatchAck];
    let matched = |path| {
        ordered
            .iter()
            .filter_map(move |s| s.ack.filter(|a| a.path == path).map(|a| (s, a)))
    };
    let [guest, dispatch] = paths.map(|path| matched(path).count());
    writeln!(
        out,
        "  shared SSIP coverage: n={n} guest-exit={guest} dispatch-ack={dispatch} unmatched={}",
        n - guest - dispatch
    )?;
    for (path, count) in paths.into_iter().zip([guest, dispatch]) {
        let rank = count.div_ceil(2);
        if count != 0 {
            // These clocks overlap: a receiver may ACK even before the
            // sender returns. Equal ticks do not determine the order.
            let (mut before, mut equal, mut after) = (0, 0, 0);
            for (_, a) in matched(path) {
                match a.ticks.cmp(&a.returned) {
                    std::cmp::Ordering::Less => before += 1,
                    std::cmp::Ordering::Equal => equal += 1,
                    std::cmp::Ordering::Greater => after += 1,
                }
            }
            writeln!(
                out,
                "  shared SSIP sender overlap: path={} n={count} ack_before_return={before} same_tick={equal} ack_after_return={after}",
                path.name()
            )?;
            let (s, a) = matched(path).nth(rank - 1).unwrap();
            writeln!(
                out,
                "  shared SSIP subset p50: path={} rank={rank}/{count} input={} sample={} rise_to_send_ticks={} send_to_ack_ticks={} ack_to_owner_ticks={} owner_to_trap_ticks={} total_ticks={} send_to_return_ticks={} ack_minus_return_ticks={}",
                path.name(),
                s.input,
                s.seq,
                a.send - s.rise,
                a.ticks - a.send,
                (s.rise + s.to_owner) - a.ticks,
                s.to_trap,
                s.total(),
                a.returned - a.send,
                i128::from(a.ticks) - i128::from(a.returned),
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_ack_requires_a_positive_ordered_interval() {
        let s = Sample::new(9, 4, 10, 30, 35).unwrap();
        let ack = |send, ticks| {
            Some(Ack {
                path: AckPath::GuestExit,
                send,
                ticks,
                returned: send,
            })
        };
        assert_eq!(s.with_ack(None).unwrap(), s);
        assert_eq!(s.with_ack(ack(10, 30)).unwrap().ack, ack(10, 30));
        for (send, ticks) in [(9, 16), (22, 22), (23, 22), (20, 31)] {
            assert!(s.with_ack(ack(send, ticks)).is_err());
        }
        assert!(
            s.with_ack(Some(Ack {
                path: AckPath::GuestExit,
                send: 11,
                ticks: 12,
                returned: 10
            }))
            .is_err()
        );
    }

    #[test]
    fn sender_duration_overlaps_delivery_and_the_signed_gap_covers_all_u64_ticks() -> Result<()> {
        for (ticks, returned, expected) in [
            (
                u64::MAX,
                0,
                "send_to_return_ticks=0 ack_minus_return_ticks=18446744073709551615",
            ),
            (
                1,
                u64::MAX,
                "send_to_return_ticks=18446744073709551615 ack_minus_return_ticks=-18446744073709551614",
            ),
            (1, 1, "send_to_return_ticks=1 ack_minus_return_ticks=0"),
        ] {
            let s = Sample::new(3, 1, 0, ticks, ticks)?.with_ack(Some(Ack {
                path: AckPath::GuestExit,
                send: 0,
                ticks,
                returned,
            }))?;
            let mut out = Vec::new();
            write_report(&mut out, &[s], 2, 0, "busy", 1)?;
            let log = std::str::from_utf8(&out)?;
            assert!(log.contains(&format!("total_ticks={ticks} {expected}")));
        }
        Ok(())
    }

    #[test]
    fn coverage_and_path_medians_never_filter_the_main_ranks() -> Result<()> {
        use AckPath::*;
        let sample = |id, trap, path: Option<AckPath>| {
            Sample::new(id + 20, id, 10, 30, trap)?.with_ack(path.map(|path| Ack {
                path,
                send: 13,
                ticks: 23,
                returned: match id {
                    2 => 23,
                    4 => 22,
                    _ => 99,
                },
            }))
        };
        let samples = [
            sample(5, 80, Some(GuestExit))?,
            sample(3, 50, None)?,
            sample(4, 60, Some(GuestExit))?,
            sample(1, 35, Some(DispatchAck))?,
            sample(2, 40, Some(GuestExit))?,
        ];
        let mut out = Vec::new();
        write_report(&mut out, &samples, 17, 1, "busy", 10_000_000)?;
        let log = std::str::from_utf8(&out)?;
        assert!(log.contains("p50 paired: input=23 sample=3 "));
        assert!(log.contains("shared SSIP coverage: n=5 guest-exit=3 dispatch-ack=1 unmatched=1"));
        assert!(
            log.contains("path=guest-exit n=3 ack_before_return=1 same_tick=1 ack_after_return=1")
        );
        assert!(
            log.contains(
                "path=dispatch-ack n=1 ack_before_return=1 same_tick=0 ack_after_return=0"
            )
        );
        assert!(log.contains("path=guest-exit rank=2/3 input=24 sample=4 rise_to_send_ticks=3 send_to_ack_ticks=10 ack_to_owner_ticks=7 owner_to_trap_ticks=30 total_ticks=50 send_to_return_ticks=9 ack_minus_return_ticks=1"));
        assert!(log.contains("path=dispatch-ack rank=1/1 input=21 sample=1 "));
        Ok(())
    }
    #[test]
    fn reported_ranks_keep_the_actual_joint_sample_even_when_totals_tie() {
        // Two events take 50 ticks with opposite dominant stages. Neither
        // can stand in for the other's input when reporting the upper quartile.
        let samples = [
            (8, 1, 80),
            (6, 1, 50),
            (1, 1, 1),
            (3, 2, 3),
            (5, 49, 50),
            (7, 70, 70),
            (2, 0, 2),
            (4, 33, 40),
        ]
        .map(|(seq, owner, trap)| Sample::new(seq + 20, seq, 0, owner, trap).unwrap());
        let mut out = Vec::new();
        write_report(&mut out, &samples, 17, 1, "busy", 10_000_000).unwrap();
        let log = std::str::from_utf8(&out).unwrap();
        assert!(log.contains("epoch=17 owner=1 mode=busy n=8 p50_ticks=40 max_ticks=80"));
        assert!(log.contains("p50 paired: input=24 sample=4 rise_to_owner_ticks=33 owner_to_trap_ticks=7 total_ticks=40 rank=4/8 equal_totals=1"));
        assert!(log.contains("p75 paired: input=26 sample=6 rise_to_owner_ticks=1 owner_to_trap_ticks=49 total_ticks=50 rank=6/8 equal_totals=2"));
        assert!(log.contains("slowest paired: input=28 sample=8 rise_to_owner_ticks=1 owner_to_trap_ticks=79 total_ticks=80 rank=8/8 equal_totals=1"));
    }

    #[test]
    fn singleton_reports_one_observation_and_empty_input_writes_no_report() {
        let mut out = Vec::new();
        assert!(write_report(&mut out, &[], 2, 0, "wait", 1).is_err());
        assert!(out.is_empty());
        let sample = Sample::new(7, 3, 5, 5, 5).unwrap();
        write_report(&mut out, &[sample], 2, 0, "wait", 1).unwrap();
        let log = std::str::from_utf8(&out).unwrap();
        assert_eq!(log.matches("input=7 sample=3").count(), 3);
        assert_eq!(log.matches("rank=1/1 equal_totals=1").count(), 3);
        assert!(log.contains("n=1 guest-exit=0 dispatch-ack=0 unmatched=1"));
        assert!(!log.contains("shared SSIP subset p50:"));
    }

    #[test]
    fn independent_stage_distributions_are_not_additive() {
        let make = |id, owner, trap| Sample::new(id + 1, id, 0, owner, trap).unwrap();
        let samples = [make(1, 60, 100), make(2, 1, 90), make(3, 10, 11)];
        assert_eq!(distribution(&samples, |s| s.to_owner), (10, 60));
        assert_eq!(distribution(&samples, |s| s.to_trap), (40, 89));
        assert_eq!(distribution(&samples, Sample::total), (90, 100));
        let worst = samples.iter().max_by_key(|s| s.total()).unwrap();
        assert_eq!(
            (worst.input, worst.seq, worst.to_owner, worst.to_trap),
            (2, 1, 60, 40)
        );
        assert!(Sample::new(2, 1, 10, 9, 10).is_err());
        assert!(Sample::new(2, 1, 10, 12, 11).is_err());
        assert_eq!(Sample::new(2, 1, 10, 10, 10).unwrap().total(), 0);
    }
}
