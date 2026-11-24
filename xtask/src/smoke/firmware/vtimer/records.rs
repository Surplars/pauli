//! Validate one complete visit from the probe's epoch-scoped console. TIME is
//! sampled by the guest; the interrupted PC does not establish physical sleep.
use anyhow::{Context, Result, ensure};
use std::io::{self, Write};

pub(super) const EVENTS: usize = 24;
const WARMUP: usize = 4;
const LEAD: u64 = 10_000; // One millisecond at the probe's 10 MHz timebase.

#[derive(Clone, Copy)]
pub(super) struct Row {
    pub(super) generation: u64,
    pub(super) due: u64,
    pub(super) armed: u64,
    pub(super) trap: u64,
    pub(super) pc: &'static str,
}

fn hex(fields: &mut std::str::Split<'_, char>, key: &str) -> Result<u64> {
    let value = fields
        .next()
        .and_then(|s| s.strip_prefix(key))
        .with_context(|| format!("missing {key} field"))?;
    ensure!(
        value.len() == 16
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid {key} value: {value:?}"
    );
    Ok(u64::from_str_radix(value, 16)?)
}

impl Row {
    fn parse(line: &str) -> Result<Self> {
        let mut fields = line.split(' ');
        ensure!(fields.next() == Some("[vt]"), "missing VSTIP record tag");
        let generation = hex(&mut fields, "gen=")?;
        let due = hex(&mut fields, "due=")?;
        let armed = hex(&mut fields, "armed=")?;
        let trap = hex(&mut fields, "trap=")?;
        let pc = match fields.next() {
            Some("pc=pre") => "pre",
            Some("pc=post") => "post",
            _ => anyhow::bail!("invalid interrupted PC label"),
        };
        ensure!(fields.next().is_none(), "extra VSTIP record fields");
        Ok(Self {
            generation,
            due,
            armed,
            trap,
            pc,
        })
    }
    pub(super) fn total(&self) -> u64 {
        self.trap - self.due
    }
}

pub(super) struct Visit(Vec<Row>);

/// Shared wire grammar for relative and absolute schedules. Schedule-specific
/// constraints are applied by the caller only after the whole visit decodes.
pub(super) fn decode(log: &[u8], first_gen: u64, count: usize) -> Result<Vec<Row>> {
    let lines = std::str::from_utf8(log)?
        .strip_suffix("\n[vt] done\n")
        .context("missing exact VSTIP visit terminator")?;
    let mut lines = lines.split('\n');
    let mut rows = Vec::with_capacity(count);
    for offset in 0..count {
        let expected = first_gen
            .checked_add(offset as u64)
            .context("VSTIP generation overflow")?;
        let row = Row::parse(lines.next().context("incomplete VSTIP visit")?)
            .with_context(|| format!("parsing VSTIP generation {expected}"))?;
        ensure!(
            row.generation == expected,
            "expected VSTIP generation {expected}, got {}",
            row.generation
        );
        ensure!(
            row.trap >= row.due.max(row.armed),
            "generation {expected} trap precedes due or arm observation"
        );
        rows.push(row);
    }
    ensure!(lines.next().is_none(), "additional data in VSTIP visit");
    Ok(rows)
}

pub(super) fn parse(log: &[u8], first_gen: u64, mut previous_trap: u64) -> Result<Visit> {
    let rows = decode(log, first_gen, EVENTS)?;
    for row in &rows {
        let expected = row.generation;
        let began = row
            .due
            .checked_sub(LEAD)
            .context("VSTIP deadline underflow")?;
        ensure!(
            began >= previous_trap,
            "generation {expected} began before previous trap"
        );
        ensure!(
            row.armed >= began,
            "generation {expected} armed before start"
        );
        previous_trap = row.trap;
    }
    Ok(Visit(rows))
}

impl Visit {
    pub(super) fn last_trap(&self) -> u64 {
        self.0[EVENTS - 1].trap
    }
    /// Common TIME window for all peers; exclude the four timer warm-ups.
    pub(super) fn scored_window(&self) -> (u64, u64) {
        (self.0[WARMUP].due - LEAD, self.last_trap())
    }
    pub(super) fn report(&self, epoch: u32, round: usize, mode: &str) -> Result<()> {
        self.write_report(&mut io::stdout().lock(), epoch, round, mode)?;
        Ok(())
    }
    fn write_report(
        &self,
        out: &mut impl Write,
        epoch: u32,
        round: usize,
        mode: &str,
    ) -> io::Result<()> {
        writeln!(
            out,
            "firmware VSTIP: epoch={epoch} round={round} mode={mode} timebase_hz=10000000 lead_ticks={LEAD} warmup={WARMUP} scored={}",
            EVENTS - WARMUP
        )?;
        for &Row {
            generation,
            due,
            armed,
            trap,
            pc,
        } in &self.0
        {
            writeln!(
                out,
                "[vt] gen={generation:016x} due={due:016x} armed={armed:016x} trap={trap:016x} pc={pc}"
            )?;
        }
        writeln!(out, "[vt] done")?;
        let mut scored = self.0[WARMUP..].to_vec();
        scored.sort_unstable_by_key(|r| (r.total(), r.generation));
        let crossed = scored.iter().filter(|r| r.armed >= r.due).count();
        let post = scored.iter().filter(|r| r.pc == "post").count();
        let n = scored.len();
        writeln!(
            out,
            "  scored: matched_gen={n} due_at_arm={crossed} pc_pre={} pc_post={post}",
            n - post
        )?;
        ranks(out, "all", &scored)?;
        scored.retain(|r| r.armed < r.due && r.pc == "post");
        writeln!(
            out,
            "  armed_before_due_pc_post={}/{n} (PC observation only)",
            scored.len()
        )?;
        ranks(out, "armed_before_due_pc_post", &scored)
    }
}

/// Each rank describes one event, including both of its additive stages.
/// Generation breaks ties reproducibly. An empty subset has no percentile.
fn ranks(out: &mut impl Write, label: &str, ordered: &[Row]) -> io::Result<()> {
    let n = ordered.len();
    if n == 0 {
        return Ok(());
    }
    for (name, rank) in [
        ("p50", n.div_ceil(2)),
        ("p95", (95 * n).div_ceil(100)),
        ("max", n),
    ] {
        let r = &ordered[rank - 1];
        writeln!(
            out,
            "  {label} {name}: rank={rank}/{n} gen={:016x} pc={} late_at_arm_ticks={} after_both_ticks={} total_ticks={}",
            r.generation,
            r.pc,
            r.armed.saturating_sub(r.due),
            r.trap - r.due.max(r.armed),
            r.total(),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
