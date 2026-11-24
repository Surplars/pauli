//! One serialized IPI round trip has three guest TIME stamps. Rank the
//! complete trip within its visit and preserve the two legs of that event.
use super::{DONE, WARMUP, decimal};
use anyhow::{Context, Result, ensure};
use std::fmt::{self, Write};

const MODES: [&str; 4] = ["busy", "wfi", "wfi", "busy"];
const SCORED: usize = 12;
const PER_VISIT: usize = WARMUP + SCORED;
const RECEIPTS: &str = "[mb] receipts ipi=64 wfi_returns=32\n";

#[derive(Clone, Copy, Default)]
struct Event {
    generation: usize,
    begin: u64,
    received: u64,
    end: u64,
}

type Visits = [[Event; PER_VISIT]; MODES.len()];

fn parse(log: &[u8], ready: &str) -> Result<Visits> {
    let body = std::str::from_utf8(log)?
        .strip_prefix(ready)
        .context("missing handoff ready line")?;
    let mut lines = body.split_inclusive('\n');
    let mut visits = Visits::default();
    let mut previous_end = 0;
    for (v, rows) in visits.iter_mut().enumerate() {
        for (i, event) in rows.iter_mut().enumerate() {
            let generation = v * PER_VISIT + i + 1;
            let row = lines.next().context("missing handoff event")?;
            let (gen_text, row) = row
                .strip_prefix("[mb] handoff gen=")
                .and_then(|r| r.strip_suffix('\n'))
                .and_then(|r| r.split_once(" mode="))
                .context("malformed handoff event")?;
            let (mode, row) = row.split_once(" begin=").context("missing handoff begin")?;
            let (begin, row) = row
                .split_once(" received=")
                .context("missing handoff receipt")?;
            let (received, end) = row.split_once(" end=").context("missing handoff end")?;
            ensure!(
                decimal(gen_text)? == generation as u64 && mode == MODES[v],
                "expected handoff generation {generation} in {} visit",
                MODES[v]
            );
            let (begin, received, end) = (decimal(begin)?, decimal(received)?, decimal(end)?);
            ensure!(
                previous_end <= begin && begin <= received && received <= end,
                "overlapping or unordered handoff TIME stamps at gen={generation}"
            );
            *event = Event {
                generation,
                begin,
                received,
                end,
            };
            previous_end = end;
        }
    }
    ensure!(
        lines.next() == Some(RECEIPTS) && lines.next() == Some(DONE) && lines.next().is_none(),
        "wrong handoff receipts, WFI returns or terminal record"
    );
    Ok(visits)
}

pub(super) fn parse_and_report(
    log: &[u8],
    ready: &str,
    epoch: u32,
    hz: u64,
    owners: [u8; 2],
) -> Result<()> {
    let visits = parse(log, ready)?;
    let mut out = String::new();
    report(&mut out, visits, epoch, hz, owners)?;
    print!("{out}");
    Ok(())
}

fn report(
    out: &mut impl Write,
    visits: Visits,
    epoch: u32,
    hz: u64,
    owners: [u8; 2],
) -> fmt::Result {
    writeln!(
        out,
        "mailbox-handoff epoch={epoch} owners={owners:?} timebase_hz={hz} receipts=64 wfi_returns=32"
    )?;
    for (v, mut rows) in visits.into_iter().enumerate() {
        let visit = v + 1;
        let mode = MODES[v];
        for (i, r) in rows.iter().enumerate() {
            let phase = if i < WARMUP { "warmup" } else { "scored" };
            writeln!(
                out,
                "mailbox-handoff sample visit={visit} mode={mode} gen={} phase={phase} begin={} received={} end={}",
                r.generation, r.begin, r.received, r.end
            )?;
        }
        let scored = &mut rows[WARMUP..];
        let longer = scored
            .iter()
            .filter(|r| r.end - r.received > r.received - r.begin)
            .count();
        writeln!(
            out,
            "mailbox-handoff visit={visit} mode={mode} n={SCORED} timebase_hz={hz} return_longer={longer}/{SCORED}"
        )?;
        scored.sort_unstable_by_key(|r| (r.end - r.begin, r.generation));
        // Nearest-rank P95 of 12 is already the maximum.
        for (rank, index) in [("p50", SCORED.div_ceil(2) - 1), ("max", SCORED - 1)] {
            let r = scored[index];
            writeln!(
                out,
                "mailbox-handoff visit={visit} mode={mode} rank={rank} gen={} outbound_ticks={} return_ticks={} total_ticks={}",
                r.generation,
                r.received - r.begin,
                r.end - r.received,
                r.end - r.begin,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
