//! Measure one periodic guest on the boot hart while another hart is overcommitted.
//! One controller owns the serial stream. Deadlines and lifecycle witnesses use
//! guest TIME (no guest offsets); host sleeps only pace queries, never prove overlap.
use super::{
    Controller, Probe, Serial, Source,
    cohort::{HZ, SCORED, STEP, WARMUP, clone_vm},
    cpu,
    multi::{Window, Worker},
    native_board,
    records::Row,
};
use anyhow::{Context, Result, ensure};
use std::time::Duration;

#[derive(Clone, Copy)]
struct Edges {
    old_live: u64,
    joined: u64,
    pre_start: u64,
    fresh_live: u64,
}

impl Edges {
    /// Classify only complete observations. The management interval need not
    /// equal CPU downtime: stop and restart each have an uncertainty bracket.
    fn counts(self, scored: &[Row]) -> Result<[usize; 3]> {
        ensure!(
            self.old_live < self.joined
                && self.joined < self.pre_start
                && self.pre_start < self.fresh_live,
            "invalid remote stop/restart TIME ordering"
        );
        let counts = [
            scored.iter().filter(|r| r.trap < self.old_live).count(),
            scored
                .iter()
                .filter(|r| self.joined < r.due && r.trap < self.pre_start)
                .count(),
            scored.iter().filter(|r| self.fresh_live < r.due).count(),
        ];
        ensure!(
            counts.iter().all(|n| *n != 0),
            "lifecycle not witnessed before/during/after within scored releases: {counts:?}"
        );
        Ok(counts)
    }
}

fn wait_until(c: &mut Controller<impl Serial>, w: &Worker, target: u64) -> Result<u64> {
    loop {
        let now = w.now(c)?;
        if now >= target {
            return Ok(now);
        }
        // Only the returned guest TIME is evidence. Bound pacing even if a
        // guest clock stalls; the controller still owns the hard deadline.
        let remaining = (target - now).min(HZ / 4);
        std::thread::sleep(Duration::from_nanos(remaining * (1_000_000_000 / HZ)));
    }
}

fn change_epoch(
    c: &mut Controller<impl Serial>,
    probe: &Probe,
    workers: &mut [Worker; 3],
    base: u64,
) -> Result<Edges> {
    let [source, other, old] = workers;
    let id = 4;
    // Stop a busy VM only after scoring has started (not during initial arming).
    let first = base
        .checked_add((WARMUP as u64 + 1) * STEP)
        .context("TIME overflow")?;
    let old_live = wait_until(c, old, first)?;
    c.command("vm stop busy3")?;
    c.terminal(id, old.epoch, "Stopped")?;
    ensure!(
        c.status(old.name, "Stopped")? == old.epoch
            && cpu(&c.log(Source::Host)[c.last_command()..], old.owner)?
                == "Stopped retired=true run=Stopped",
        "old worker has not joined"
    );
    let joined = source.now(c)?;
    // Keep the lane joined for two releases. A complete due/trap pair inside
    // these timestamps proves the timer ran, not just that both endpoints exist.
    let pre_start = wait_until(
        c,
        source,
        joined.checked_add(2 * STEP).context("TIME overflow")?,
    )?;
    c.command("vm start busy3")?;
    let fresh = Worker::ready(c, id, old.name, old.owner)?;
    ensure!(
        ![probe.epoch, source.epoch, other.epoch, old.epoch].contains(&fresh.epoch),
        "cold worker reused a live or stopped epoch"
    );
    ensure!(
        fresh.end(c)? == Window::default(),
        "fresh worker retained old work"
    );
    fresh.begin(c)?;
    let fresh_live = fresh.now(c)?;
    println!(
        "smp-timer restart vm={id} old={} fresh={} owner={} old_live={old_live} joined={joined} pre_start={pre_start} fresh_live={fresh_live}",
        old.epoch, fresh.epoch, fresh.owner
    );
    *old = fresh;
    Ok(Edges {
        old_live,
        joined,
        pre_start,
        fresh_live,
    })
}

fn visit(
    c: &mut Controller<impl Serial>,
    probe: &mut Probe,
    workers: &mut [Worker; 3],
    mode: Mode,
) -> Result<()> {
    let label = mode.label();
    let busy = mode != Mode::Idle;
    for w in workers.iter() {
        if busy { w.begin(c)? } else { w.parked(c)? }
    }
    let now = probe.command_now(c)?;
    let base = now.checked_add(HZ).context("first compare overflow")?;
    probe.queue(c, base)?;
    let edges = if mode == Mode::Restart {
        Some(change_epoch(c, probe, workers, base)?)
    } else {
        None
    };
    let rows = probe.collect_grid(c, base, now)?;
    let scored = &rows[WARMUP..WARMUP + SCORED];
    let start = scored[0].due;
    let end = scored[SCORED - 1].trap;
    if let Some(e) = edges {
        let counts = e.counts(scored)?;
        println!(
            "smp-timer witnessed scored_before={} scored_joined={} scored_after={}",
            counts[0], counts[1], counts[2]
        );
    }
    for (i, w) in workers.iter().enumerate() {
        let record = w.end(c)?;
        if busy {
            // A restarted worker can attest only its own epoch, never the
            // entire visit. The two surviving workers enclose the full grid.
            let first = edges.filter(|_| i == 2).map_or(start, |e| e.fresh_live);
            let n = record
                .progress_inside((first, end))
                .with_context(|| w.name)?;
            println!(
                "smp-timer {label} worker={} epoch={} owner={} scored={first}..{end} outer={record:?} min_checkpoints={n}",
                w.name, w.epoch, w.owner
            );
        } else {
            ensure!(record == Window::default(), "parked {} did work", w.name);
        }
    }
    let mut late = [0; SCORED];
    for (i, r) in rows.iter().enumerate() {
        if (WARMUP..WARMUP + SCORED).contains(&i) {
            late[i - WARMUP] = r.total();
        }
        println!(
            "smp-timer row mode={label} gen={} due={} armed={} trap={} pc={}",
            r.generation, r.due, r.armed, r.trap, r.pc
        );
    }
    late.sort_unstable();
    println!(
        "smp-timer mode={label} timer_epoch={} owner={} scored={SCORED} p50_ticks={} p95_ticks={} max_ticks={}",
        probe.epoch,
        probe.owner,
        late[SCORED.div_ceil(2) - 1],
        late[(95 * SCORED).div_ceil(100) - 1],
        late[SCORED - 1]
    );
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Idle,
    Busy,
    Restart,
}
impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Busy => "busy",
            Self::Restart => "restart",
        }
    }
}

pub(in crate::smoke::firmware) fn verify_smp(c: &mut Controller<impl Serial>) -> Result<()> {
    let (boot, hz) = native_board(c.log(Source::Host), 2)?;
    ensure!(hz == HZ, "SMP timer probe expects {HZ} Hz TIME");
    let remote = 1 - boot;
    let mut probe = Probe::start_vm(c, 1, "control", boot)?;
    let source = Worker::ready(c, 2, "service", remote)?;
    clone_vm(c, "service", "busy2", 3)?;
    let second = Worker::ready(c, 3, "busy2", remote)?;
    clone_vm(c, "service", "busy3", 4)?;
    let third = Worker::ready(c, 4, "busy3", remote)?;
    let mut epochs = [probe.epoch, source.epoch, second.epoch, third.epoch];
    epochs.sort_unstable();
    ensure!(
        epochs.windows(2).all(|w| w[0] != w[1]),
        "VMs share an epoch"
    );
    let mut workers = [source, second, third];
    for mode in [Mode::Idle, Mode::Busy, Mode::Restart] {
        visit(c, &mut probe, &mut workers, mode)
            .with_context(|| format!("SMP timer {}", mode.label()))?;
    }
    probe.shutdown(c)?;
    for (id, w) in [(3, &workers[1]), (4, &workers[2])] {
        c.command(&format!("vm stop {}", w.name))?;
        c.terminal(id, w.epoch, "Stopped")?;
        c.destroy(id, w.name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
