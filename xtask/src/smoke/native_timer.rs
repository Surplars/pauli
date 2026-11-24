//! Validate the OpenSBI-only probe's evidence, not a latency threshold.
//! Every sample is early or belongs to one lateness bin, never both.
use super::{Controller, controller::Serial};
use anyhow::{Context, Result, ensure};

#[derive(Default)]
struct Windows {
    round: u64,
    hz: u64,
    end: u64,
}

impl Windows {
    /// Only a complete four-window sequence may finish the probe.
    fn accept(&mut self, line: &str) -> Result<bool> {
        if line == "complete" {
            ensure!(
                self.round == 4,
                "native timer completed before all four windows"
            );
            return Ok(true);
        }
        let mut fields = line.split_ascii_whitespace();
        let mut number = |name: &str| -> Result<u64> {
            let (key, value) = fields
                .next()
                .and_then(|s| s.split_once('='))
                .context("missing native timer field")?;
            ensure!(key == name, "expected native timer field {name}, got {key}");
            Ok(value.parse()?)
        };
        let round = number("round")?;
        let hz = number("hz")?;
        let start = number("start_ticks")?;
        let end = number("end_ticks")?;
        let samples = number("samples")?;
        let early = number("early")?;
        let arm_late = number("arm_ge10ms")?;
        let bins = [
            number("lt1ms")?,
            number("lt5ms")?,
            number("lt10ms")?,
            number("ge10ms")?,
        ];
        ensure!(fields.next().is_none(), "unexpected native timer field");
        ensure!(
            round == self.round + 1 && round <= 4,
            "out-of-order native timer round"
        );
        ensure!(
            hz != 0 && (self.round == 0 || self.hz == hz),
            "invalid or changing native timer frequency"
        );
        // Actual readings bracket the absolute 2/4/6/8-second boundaries. Do
        // not turn an overshoot into an SLA failure or require start == 0.
        let boundary = 2 * u128::from(hz) * u128::from(round);
        ensure!(
            start >= self.end && u128::from(start) < boundary && boundary <= u128::from(end),
            "invalid native timer window for round {round}"
        );
        let expired: u128 = bins.into_iter().map(u128::from).sum();
        ensure!(
            expired != 0
                && u128::from(samples) == u128::from(early) + expired
                && arm_late <= bins[3],
            "inconsistent native timer sample counts for round {round}"
        );
        *self = Self { round, hz, end };
        Ok(false)
    }
}

pub(super) fn verify<T: Serial>(c: &mut Controller<T>) -> Result<()> {
    let mut windows = Windows::default();
    let mut cursor = 0;
    while let Some(line) = c.native_line(&mut cursor)? {
        // Plain OpenSBI console bytes decode as Host; no monitor command or
        // source-switch frame is present. Commit only full LF/CRLF lines.
        let Some(raw) = line.strip_prefix(b"native-timer ") else {
            continue;
        };
        let line = std::str::from_utf8(raw)?;
        if windows.accept(line)? {
            return Ok(());
        }
        println!("native-timer {line}");
    }
    anyhow::bail!("native timer exited before completion")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: &str = "round=1 hz=1000 start_ticks=91 end_ticks=2013 samples=15 early=3 arm_ge10ms=1 lt1ms=5 lt5ms=1 lt10ms=2 ge10ms=4";

    #[test]
    fn accepts_absolute_windows_with_overshoot_and_nonzero_start() {
        let mut w = Windows::default();
        assert!(!w.accept(FIRST).unwrap());
        for round in 2..=4 {
            let row = format!(
                "round={round} hz=1000 start_ticks={} end_ticks={} samples=1 early=0 arm_ge10ms=1 lt1ms=0 lt5ms=0 lt10ms=0 ge10ms=1",
                w.end + 2,
                round * 2000 + 899
            );
            w.accept(&row).unwrap();
        }
        assert!(w.accept("complete").unwrap());
    }

    #[test]
    fn plain_split_serial_needs_four_windows_and_a_complete_final_line() {
        use crate::process::Output;
        use std::time::Duration;
        struct Raw<'a>(std::slice::Chunks<'a, u8>);
        impl Serial for Raw<'_> {
            fn send(&mut self, _: &[u8]) -> Result<()> {
                anyhow::bail!("the native payload accepts no input")
            }
            fn receive(&mut self, _: Duration) -> Result<Output> {
                Ok(self
                    .0
                    .next()
                    .map_or(Output::End, |b| Output::Data(b.to_vec())))
            }
        }
        let mut log = String::from("OpenSBI boot\r\n");
        for r in 1..=4 {
            log += &format!(
                "native-timer round={r} hz=1000 start_ticks={} end_ticks={} samples=1 early=0 arm_ge10ms=0 lt1ms=1 lt5ms=0 lt10ms=0 ge10ms=0\r\n",
                (r - 1) * 2000 + 11,
                r * 2000
            );
        }
        log += "native-timer complete\r\n";
        let check = |s: &str| {
            verify(&mut Controller::new(
                Raw(s.as_bytes().chunks(7)),
                Duration::from_secs(2),
            ))
        };
        assert!(check(&log).is_ok());
        assert!(check(log.trim_end()).is_err());
    }

    #[test]
    fn rejects_duplicate_skipped_or_overlapping_windows_and_invalid_counts() {
        let mut w = Windows::default();
        assert!(w.accept("complete").is_err());
        assert!(w.accept("round=1 hz=1000 start_ticks=91 end_ticks=2013 samples=3 early=3 arm_ge10ms=0 lt1ms=0 lt5ms=0 lt10ms=0 ge10ms=0").is_err());
        for (before, after) in [
            ("round=1", "round=2"),
            ("hz=1000", "hz=0"),
            ("end_ticks=2013", "end_ticks=1999"),
            ("start_ticks=91", "start_ticks=2000"),
            ("samples=15", "samples=12"),
            ("arm_ge10ms=1", "arm_ge10ms=5"),
            ("early=3", "early=18446744073709551615"),
            (" lt1ms=5", ""),
            ("ge10ms=4", "ge10ms=4 extra=0"),
        ] {
            assert!(w.accept(&FIRST.replace(before, after)).is_err(), "{after}");
        }
        w.accept(FIRST).unwrap();
        assert!(w.accept(FIRST).is_err());
        let next = FIRST
            .replace("round=1", "round=2")
            .replace("end_ticks=2013", "end_ticks=4005");
        assert!(w.accept(&next).is_err()); // Overlaps the first window.
        assert!(
            w.accept(
                &next
                    .replace("start_ticks=91", "start_ticks=2015")
                    .replace("hz=1000", "hz=999")
            )
            .is_err()
        );
    }
}
