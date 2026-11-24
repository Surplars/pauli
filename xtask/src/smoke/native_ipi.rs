//! Check one OpenSBI send-to-SSIP-entry timeline; no VM or serial input participates.
//! Ownership: a single sender prints receiver evidence after an acknowledged sample.
//! Invariants: four complete ABBA blocks, no overlapping samples, no latency threshold.
use super::{Controller, controller::Serial};
use anyhow::{Context, Result, ensure};
use std::str::SplitAsciiWhitespace;

const BLOCK: usize = 8;
const MODES: [&str; 4] = ["wait", "busy", "busy", "wait"];
const SAMPLES: usize = BLOCK * MODES.len();

struct Sample {
    delivery: u64,
    sbi: u64,
}

#[derive(Default)]
struct Run {
    harts: Option<(u64, u64, u64)>, // Sender, receiver and shared TIME frequency.
    samples: Vec<Sample>,
    previous_end: u64,
    resumed: usize,
    complete: bool,
}

fn field<'a>(words: &mut SplitAsciiWhitespace<'a>, name: &str) -> Result<&'a str> {
    words
        .next()
        .and_then(|s| s.strip_prefix(name))
        .with_context(|| format!("missing native IPI field {name}"))
}

impl Run {
    fn accept(&mut self, line: &str) -> Result<()> {
        ensure!(!self.complete, "native IPI record after completion");
        if self.harts.is_none() {
            let mut words = line
                .strip_prefix("ready ")
                .context("missing native IPI ready")?
                .split_ascii_whitespace();
            let sender = field(&mut words, "sender=")?.parse()?;
            let receiver = field(&mut words, "receiver=")?.parse()?;
            let hz = field(&mut words, "hz=")?.parse()?;
            ensure!(
                sender < 2
                    && receiver < 2
                    && sender != receiver
                    && hz != 0
                    && words.next().is_none(),
                "invalid two-hart IPI topology: {line}"
            );
            self.harts = Some((sender, receiver, hz));
            return Ok(());
        }
        let index = self.samples.len();
        if line == "complete" {
            ensure!(
                index == SAMPLES,
                "incomplete native IPI samples: {index}/{SAMPLES}"
            );
            self.complete = true;
            return Ok(());
        }
        let mut words = line.split_ascii_whitespace();
        let seq: usize = field(&mut words, "seq=")?.parse()?;
        let mode = field(&mut words, "mode=")?;
        let send: u64 = field(&mut words, "send=")?.parse()?;
        let returned: u64 = field(&mut words, "returned=")?.parse()?;
        let trap: u64 = field(&mut words, "trap=")?.parse()?;
        let site = field(&mut words, "site=")?;
        let work: u64 = field(&mut words, "work=")?.parse()?;
        ensure!(
            index < SAMPLES && seq == index + 1 && mode == MODES[index / BLOCK],
            "out-of-order native IPI sample: {line}"
        );
        ensure!(
            words.next().is_none() && self.previous_end <= send && send <= trap && send <= returned,
            "overlapping or invalid native IPI timestamps: {line}"
        );
        match (mode, site, work) {
            ("busy", "busy", 1..) | ("wait", "wait-armed", 0) => {}
            ("wait", "wait-resume", 0) => self.resumed += 1,
            _ => anyhow::bail!("native IPI trap site or work disagrees with mode: {line}"),
        }
        self.samples.push(Sample {
            delivery: trap - send,
            sbi: returned - send,
        });
        self.previous_end = trap.max(returned);
        Ok(())
    }

    fn report(&self) -> Result<()> {
        ensure!(self.complete, "native IPI ended without complete evidence");
        let (sender, receiver, hz) = self.harts.context("missing native IPI topology")?;
        for mode in ["wait", "busy"] {
            let (mut ticks, mut requests): (Vec<_>, Vec<_>) = self
                .samples
                .iter()
                .enumerate()
                .filter_map(|(i, s)| (MODES[i / BLOCK] == mode).then_some((s.delivery, s.sbi)))
                .unzip();
            // Both intervals start at send: the receiver can trap before the
            // sender returns from SBI. Count pairs before independently sorting.
            let early = ticks
                .iter()
                .zip(&requests)
                .filter(|(trap, ret)| trap <= ret)
                .count();
            ticks.sort_unstable();
            requests.sort_unstable();
            let n = ticks.len(); // Exact ABBA block lengths were checked at completion.
            println!(
                "native IPI send->S trap: sender={sender} receiver={receiver} mode={mode} n={n} p50_ticks={} max_ticks={} timebase_hz={hz}",
                ticks[(n - 1) / 2],
                ticks[n - 1]
            );
            println!(
                "  send->SBI return p50_ticks={} max_ticks={} trap_at_or_before_return={early}/{n}",
                requests[(n - 1) / 2],
                requests[n - 1]
            );
            // Keep the matching sender duration, not the independently sorted maximum.
            if let Some((i, worst)) = self
                .samples
                .iter()
                .enumerate()
                .find(|(i, s)| MODES[i / BLOCK] == mode && s.delivery == ticks[n - 1])
            {
                println!(
                    "  slowest delivery: seq={} trap_ticks={} return_ticks={}",
                    i + 1,
                    worst.delivery,
                    worst.sbi
                );
            }
            if mode == "wait" {
                println!(
                    "  wait sites: armed={} resumed={}",
                    n - self.resumed,
                    self.resumed
                );
            }
        }
        Ok(())
    }
}

pub(super) fn verify<T: Serial>(c: &mut Controller<T>) -> Result<()> {
    let mut run = Run::default();
    let mut cursor = 0;
    while let Some(line) = c.native_line(&mut cursor)? {
        // Ignore the firmware banner, never another sample merely because it
        // arrived after the success marker in a later serial chunk.
        if let Some(raw) = line.strip_prefix(b"native-ipi ") {
            let row = std::str::from_utf8(raw)?;
            run.accept(row)
                .with_context(|| format!("native-ipi {row}"))?;
        } else if run.harts.is_some() {
            ensure!(
                line.is_empty(),
                "unexpected output from native IPI payload: {}",
                String::from_utf8_lossy(line)
            );
        }
    }
    run.report()
}

#[cfg(test)]
mod tests;
