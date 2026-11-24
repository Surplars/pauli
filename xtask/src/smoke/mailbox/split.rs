//! Join immutable guest TIME windows to the target owner's fence completions.
//! Only a fresh native reply for the same epoch/placement may supply the midpoint.
//! The single requester serializes calls; each fence kind has its own generation.
use super::{N, WARMUP, Window, decimal};
use anyhow::{Context, Result, ensure};
use std::fmt::{self, Write};

const OPS: [&str; 2] = ["vma", "fencei"];
const COUNT: usize = WARMUP + N;
const PATHS: [&str; 2] = ["direct", "deferred"];

#[derive(Clone, Copy, Default)]
struct Completion {
    ticks: u64,
    // An empty slot was never parsed. Neither a missing label nor
    // "unobserved" is evidence that the requester completed directly.
    path: &'static str,
}

#[derive(Clone, Copy, Default)]
struct Event {
    generation: usize,
    begin: u64,
    completed: u64,
    end: u64,
    path: &'static str,
}

fn completions(reply: &[u8], epoch: u32, owner: u8) -> Result<[[Completion; COUNT]; 2]> {
    // Command echo ended before the reply cursor; monitor emits a newline and a fresh prompt.
    let body = std::str::from_utf8(reply)?
        .strip_suffix("hv> ")
        .context("incomplete native fence reply")?
        .trim_matches(['\r', '\n']);
    let prefix = format!("fence epoch={epoch} target=1 owner={owner} requester=0 op=");
    let mut table = [[Completion::default(); COUNT]; 2];
    for line in body.lines() {
        let (op, ticket) = line
            .strip_prefix(&prefix)
            .context("wrong native fence source")?
            .split_once(" gen=")
            .context("missing fence generation")?;
        let (gen_text, tail) = ticket
            .split_once(" ticks=")
            .context("missing fence completion")?;
        let (ticks, value) = tail
            .split_once(" path=")
            .context("missing requester path")?;
        let path = PATHS
            .into_iter()
            .find(|&s| s == value)
            .context("unclassified requester path")?;
        let generation = usize::try_from(decimal(gen_text)?)?;
        let i = OPS
            .iter()
            .position(|&s| s == op)
            .context("wrong fence kind")?;
        ensure!((1..=COUNT).contains(&generation), "wrong fence generation");
        let slot = &mut table[i][generation - 1];
        ensure!(slot.path.is_empty(), "duplicate native fence completion");
        *slot = Completion {
            ticks: decimal(ticks)?,
            path,
        };
    }
    ensure!(
        table.iter().flatten().all(|c| !c.path.is_empty()),
        "missing native fence completion"
    );
    Ok(table)
}

fn join(
    reply: &[u8],
    epoch: u32,
    owner: u8,
    windows: &[[Window; N]; 3],
) -> Result<[[Event; N]; 2]> {
    let seen = completions(reply, epoch, owner)?;
    // Pre-scoring rounds were not exported by the guest. Check that all eight
    // native warm-up completions precede every scored call, in issue order.
    let first = windows[WARMUP % 3][0].start; // first operation in the scored rotation
    let mut prior = 0;
    for round in 0..WARMUP {
        for offset in 0..3 {
            let op = (round + offset) % 3;
            if op != 0 {
                let tick = seen[op - 1][round].ticks;
                ensure!(
                    prior <= tick && tick <= first,
                    "native warm-up order mismatch"
                );
                prior = tick;
            }
        }
    }
    let mut joined = [[Event::default(); N]; 2];
    for (op, rows) in joined.iter_mut().enumerate() {
        for (n, row) in rows.iter_mut().enumerate() {
            let generation = WARMUP + n + 1;
            let window = windows[op + 1][n];
            let end = window
                .start
                .checked_add(window.ticks)
                .context("guest TIME overflow")?;
            let Completion {
                ticks: completed,
                path,
            } = seen[op][generation - 1];
            ensure!(
                (window.start..=end).contains(&completed),
                "{} gen={generation}: completion outside guest TIME window",
                OPS[op]
            );
            *row = Event {
                generation,
                begin: window.start,
                completed,
                end,
                path,
            };
        }
    }
    Ok(joined)
}

pub(super) fn report(
    reply: &[u8],
    epoch: u32,
    owner: u8,
    windows: &[[Window; N]; 3],
) -> Result<()> {
    let joined = join(reply, epoch, owner, windows)?;
    let mut out = String::new();
    for (op, mut rows) in OPS.into_iter().zip(joined) {
        let mut longer = 0;
        for r in rows {
            longer += usize::from(r.end - r.completed > r.completed - r.begin);
            writeln!(
                out,
                "mailbox-split sample epoch={epoch} target=1 owner={owner} requester=0 op={op} gen={} begin={} completed={} end={} path={}",
                r.generation, r.begin, r.completed, r.end, r.path
            )?;
        }
        writeln!(
            out,
            "mailbox-split op={op} n={N} return_longer={longer}/{N}"
        )?;
        rows.sort_unstable_by_key(|r| (r.end - r.begin, r.generation));
        ranked(&mut out, op, None, &rows)?;
        for path in PATHS {
            // Filtering preserves the total-latency order, including tie breaks.
            let group: Vec<_> = rows.iter().copied().filter(|r| r.path == path).collect();
            ranked(&mut out, op, Some(path), &group)?;
        }
    }
    print!("{out}");
    Ok(())
}

/// Report both legs of the same ranked event, never independent percentiles.
/// An empty path still has a useful count, but has no possible rank.
fn ranked(out: &mut impl Write, op: &str, subset: Option<&str>, sorted: &[Event]) -> fmt::Result {
    let n = sorted.len();
    let tag = match subset {
        Some(path) => {
            writeln!(out, "mailbox-split group op={op} path={path} n={n}")?;
            "mailbox-split group"
        }
        None => "mailbox-split",
    };
    if n == 0 {
        return Ok(());
    }
    for (rank, index) in [
        ("p50", n.div_ceil(2)),
        ("p95", (95 * n).div_ceil(100)),
        ("max", n),
    ] {
        if subset.is_some() && rank == "p95" {
            continue;
        }
        let r = sorted[index - 1];
        writeln!(
            out,
            "{tag} op={op} rank={rank} gen={} outward_ticks={} return_ticks={} total_ticks={} path={}",
            r.generation,
            r.completed - r.begin,
            r.end - r.completed,
            r.end - r.begin,
            r.path
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
