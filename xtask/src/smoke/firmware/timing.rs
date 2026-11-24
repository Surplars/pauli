//! Join each private RTOS histogram with the ordinary report for the same releases.
//! The serial controller owns the cursor; no locks. Late observations and skipped
//! periods are different quantities: one very late observation may skip several.
use super::{Boundary, Controller, Report, Serial, Source, line, metrics::WINDOW, window};
use anyhow::{Context, Result, ensure};

const LATENCY: &str = "[rtos] latency samples=";
const LABELS: [&str; 4] = ["lt2ms=", "lt10ms=", "lt20ms=", "ge20ms="];

pub(super) fn require_probe(log: &[u8]) -> Result<()> {
    let (boot, _) = std::str::from_utf8(log)?
        .split_once("[rtos] scheduler ready hz=")
        .context("missing RTOS readiness")?;
    ensure!(
        boot.lines().any(|s| s.ends_with("[rtos] timing-v1")),
        "timing smoke needs a current vs-control; rebuild openion-vs-example"
    );
    Ok(())
}

pub(super) fn pair<T: Serial>(
    c: &mut Controller<T>,
    epoch: u32,
    cursor: &mut usize,
    anchor: Boundary,
    hz: u32,
) -> Result<(Boundary, [u64; 2], [u64; 4])> {
    let start = *cursor;
    let first = window(c, epoch, cursor, anchor)?;
    let second = window(c, epoch, cursor, first.end)?;
    ensure!(
        !std::str::from_utf8(&c.log(Source::Guest(epoch))[start..*cursor])?.contains(LATENCY),
        "RTOS emitted histogram inside scored interval"
    );
    let mut total = [0; 4];
    // The guest emits both histograms after the second ordinary report so that
    // diagnostic UART writes cannot delay a release inside the scored pair.
    for (before, report) in [(anchor, &first), (first.end, &second)] {
        let row = line(c, Source::Guest(epoch), cursor, LATENCY)?;
        let bins = histogram(row, before, report, hz)?;
        for (sum, bin) in total.iter_mut().zip(bins) {
            *sum += bin;
        }
    }
    Ok((second.end, [first.peak, second.peak], total))
}

fn histogram(row: &str, before: Boundary, report: &Report, hz: u32) -> Result<[u64; 4]> {
    fn count(s: &str) -> Result<u64> {
        ensure!(s.bytes().all(|b| b.is_ascii_digit()), "invalid RTOS count");
        Ok(s.parse()?)
    }
    let mut fields = row.split(' ');
    ensure!(
        count(fields.next().context("missing histogram ID")?)? == report.end.samples,
        "RTOS histogram is from a different release window"
    );
    let mut bins = [0; 4];
    for (bin, label) in bins.iter_mut().zip(LABELS) {
        let value = fields
            .next()
            .and_then(|s| s.strip_prefix(label))
            .context("bad RTOS histogram field")?;
        *bin = count(value)?;
        ensure!(*bin <= WINDOW, "histogram bin exceeds window size");
    }
    ensure!(
        fields.next().is_none() && bins.iter().sum::<u64>() == WINDOW,
        "incomplete RTOS histogram"
    );
    // Check the independent maximum against exact wall-time thresholds; tick
    // rounding at non-decimal clock frequencies must agree with the guest.
    let top =
        [2, 10, 20].partition_point(|ms| u128::from(report.peak) * 1000 >= u128::from(hz) * ms);
    ensure!(
        bins.iter().rposition(|&n| n != 0) == Some(top),
        "RTOS bins disagree with window maximum"
    );
    let missed = report.end.missed - before.missed; // window() checked monotonicity.
    ensure!(bins[3] <= missed, "long releases exceed skipped periods");
    if hz.is_multiple_of(50) {
        // At these clock rates the 20 ms bin boundary is exactly one period.
        let max_skips = report.peak / u64::from(hz / 50);
        ensure!(
            u128::from(missed) <= u128::from(bins[3]) * u128::from(max_skips)
                && u128::from(max_skips) + u128::from(bins[3].saturating_sub(1))
                    <= u128::from(missed),
            "RTOS missed-period counter disagrees with window maximum"
        );
    }
    Ok(bins)
}

#[cfg(test)]
mod tests;
