//! Two independent VS epochs receive exactly the same absolute timer grid.
//! The controller owns serial setup; guests buffer results without locks or UART
//! during scoring. Only a fully paired, early-armed visit can produce statistics.
use super::{
    Controller, DONE, Probe, Serial, Source,
    multi::{Window, Worker},
    native_board,
    records::{self, Row},
};
use anyhow::{Context, Result, ensure};

pub(super) const HZ: u64 = 10_000_000;
pub(super) const STEP: u64 = 100_000;
pub(super) const WARMUP: usize = 4;
pub(super) const SCORED: usize = 20;
const COUNT: usize = WARMUP + SCORED + 1;
const GUARD: usize = COUNT - 1;

impl Probe {
    pub(super) fn command_now(&mut self, c: &mut Controller<impl Serial>) -> Result<u64> {
        let src = Source::Guest(self.epoch);
        c.attach(self.name, self.epoch)?;
        c.guest(self.epoch, b'n', "\n")?;
        c.detach()?;
        let log = c.log(src);
        let stamp = std::str::from_utf8(&log[self.cursor..])?
            .strip_prefix("[vt] now=")
            .and_then(|s| s.strip_suffix('\n'))
            .context("invalid fresh TIME reply")?;
        let now = super::super::work(stamp)?;
        ensure!(
            now >= self.last_trap,
            "guest clock regressed between visits"
        );
        self.cursor = log.len();
        Ok(now)
    }

    pub(super) fn queue(&mut self, c: &mut Controller<impl Serial>, base: u64) -> Result<()> {
        let src = Source::Guest(self.epoch);
        let digits = format!("{base:016x}");
        let ack = format!("[vt] arm\n{digits}\n[vt] queued\n");
        c.attach(self.name, self.epoch)?;
        c.guest(self.epoch, b'a', "[vt] arm\n")?;
        // One echoed nibble per input byte, as with host console commands:
        // an OS pipe accepting bytes does not prove the guest received them.
        for (offset, key) in digits.bytes().enumerate() {
            c.guest(self.epoch, key, &digits[offset..offset + 1])?;
        }
        c.wait(src, self.cursor, "[vt] queued\n")?;
        c.detach()?;
        ensure!(
            c.log(src)[self.cursor..].starts_with(ack.as_bytes()),
            "wrong grid acknowledgement"
        );
        self.cursor += ack.len();
        Ok(())
    }

    pub(super) fn collect_grid(
        &mut self,
        c: &mut Controller<impl Serial>,
        base: u64,
        now: u64,
    ) -> Result<Vec<Row>> {
        let src = Source::Guest(self.epoch);
        c.wait(src, self.cursor, DONE)?;
        let log = c.log(src);
        let rows = decode(
            &log[self.cursor..],
            self.next_gen,
            base,
            self.last_trap.max(now),
        )?;
        self.cursor = log.len();
        self.next_gen += COUNT as u64;
        self.last_trap = rows[GUARD].trap;
        Ok(rows)
    }
}

fn decode(log: &[u8], first_gen: u64, base: u64, mut previous: u64) -> Result<Vec<Row>> {
    let rows = records::decode(log, first_gen, COUNT)?;
    for (i, row) in rows.iter().enumerate() {
        let due = base.checked_add(i as u64 * STEP).context("grid overflow")?;
        ensure!(
            row.due == due && previous <= row.armed && row.armed < due,
            "generation {}: expected due {due}, got due {} armed {} after {previous}",
            row.generation,
            row.due,
            row.armed
        );
        previous = row.trap;
    }
    Ok(rows)
}

struct Paired {
    a: Vec<Row>,
    b: Vec<Row>,
}
impl Paired {
    fn window(&self) -> (u64, u64) {
        let last = GUARD - 1;
        (self.a[WARMUP].due, self.a[last].trap.max(self.b[last].trap))
    }
    fn report(&self, round: usize, busy: bool, epochs: [u32; 2]) {
        // Both sources independently passed the exact same absolute grid.
        // The final guard was armed before due by both, so its UART dump
        // cannot delay the last scored trap on the other guest.
        let mut stats = [[0u64; SCORED]; 3];
        for (i, (a, b)) in self.a.iter().zip(&self.b).enumerate() {
            println!(
                "cohort pair round={round} gen={} due={} arm_a={} trap_a={} pc_a={} arm_b={} trap_b={} pc_b={}",
                a.generation, a.due, a.armed, a.trap, a.pc, b.armed, b.trap, b.pc
            );
            if (WARMUP..GUARD).contains(&i) {
                for (series, value) in
                    stats
                        .iter_mut()
                        .zip([a.total(), b.total(), a.trap.abs_diff(b.trap)])
                {
                    series[i - WARMUP] = value;
                }
            }
        }
        // A percentile is computed separately for each VM and for paired skew.
        for (metric, mut series) in ["late_a", "late_b", "pair_skew"].into_iter().zip(stats) {
            series.sort_unstable();
            println!(
                "cohort round={round} busy={busy} epochs={epochs:?} metric={metric} n={SCORED} p50_ticks={} p95_ticks={} max_ticks={}",
                series[SCORED.div_ceil(2) - 1],
                series[(95 * SCORED).div_ceil(100) - 1],
                series[SCORED - 1]
            );
        }
    }
}

fn round(
    c: &mut Controller<impl Serial>,
    timers: &mut [Probe; 2],
    workers: &[Worker; 3],
    round: usize,
    busy: bool,
    reverse: bool,
) -> Result<()> {
    for w in workers {
        if busy { w.begin(c)? } else { w.parked(c)? }
    }
    let now = timers[0].command_now(c)?;
    let base = now.checked_add(HZ).context("first compare overflow")?;
    for i in if reverse { [1, 0] } else { [0, 1] } {
        timers[i].queue(c, base)?;
    }
    let pair = Paired {
        a: timers[0].collect_grid(c, base, now)?,
        b: timers[1].collect_grid(c, base, now)?,
    };
    for w in workers {
        let record = w.end(c)?;
        if busy {
            let count = record
                .progress_inside(pair.window())
                .with_context(|| w.name)?;
            println!(
                "cohort round={round} worker={} epoch={} scored_ticks={:?} outer={record:?} scored_min_checkpoints={count}",
                w.name,
                w.epoch,
                pair.window()
            );
        } else {
            ensure!(
                record == Window::default(),
                "parked {} reported work",
                w.name
            );
        }
    }
    pair.report(round, busy, [timers[0].epoch, timers[1].epoch]);
    Ok(())
}

pub(super) fn clone_vm(
    c: &mut Controller<impl Serial>,
    template: &str,
    name: &str,
    id: u32,
) -> Result<()> {
    c.command(&format!("vm create {template} {name}"))?;
    c.wait(
        Source::Host,
        c.last_command(),
        &format!("VM {id} created: {name} (stopped)\n"),
    )?;
    c.command(&format!("vm start {name}"))
}

pub(in crate::smoke::firmware) fn verify_cohort(c: &mut Controller<impl Serial>) -> Result<()> {
    let (_, hz) = native_board(c.log(Source::Host), 1)?;
    ensure!(hz == HZ, "coincident timer firmware expects {HZ} Hz TIME");
    let a = Probe::start(c)?;
    let source = Worker::ready(c, 2, "service", 0)?;
    // Adverse RR placement with two busy lanes between timers; the fifth VM
    // also forces the ordinary rotation to cross the placement group boundary.
    clone_vm(c, "service", "busy2", 3)?;
    let second = Worker::ready(c, 3, "busy2", 0)?;
    clone_vm(c, "control", "timer2", 4)?;
    let b = Probe::start_vm(c, 4, "timer2", 0)?;
    clone_vm(c, "service", "busy3", 5)?;
    let third = Worker::ready(c, 5, "busy3", 0)?;
    let mut epochs = [a.epoch, b.epoch, source.epoch, second.epoch, third.epoch];
    epochs.sort_unstable();
    ensure!(epochs.windows(2).all(|w| w[0] != w[1]), "shared VM epoch");
    let mut timers = [a, b];
    let workers = [source, second, third];
    for (i, (busy, reverse)) in [(false, false), (true, false), (true, true)]
        .into_iter()
        .enumerate()
    {
        round(c, &mut timers, &workers, i + 1, busy, reverse)
            .with_context(|| format!("timer cohort visit {}", i + 1))?;
    }
    for timer in &timers {
        timer.shutdown(c)?;
    }
    c.destroy(4, "timer2")?;
    for (id, w) in [(3, &workers[1]), (5, &workers[2])] {
        c.command(&format!("vm stop {}", w.name))?;
        c.terminal(id, w.epoch, "Stopped")?;
        c.destroy(id, w.name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
