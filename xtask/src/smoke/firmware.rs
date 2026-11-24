//! Diskless firmware: shared-hart timing, or UART wake and restart on one or two harts.
//! Ownership: one controller, one hard deadline, serial input bound to the attached epoch.
//! Invariants: score only complete windows after a causal guest ACK, never a timing SLA.
mod irq;
mod metrics;
mod park;
mod timing;
mod vtimer;
use super::{Controller, Source, controller::Serial, respond};
use anyhow::{Context, Result, ensure};
pub(super) use irq::latency::Capture;
use metrics::{Boundary, Report, Score, work};

const REPORT: &str = "[rtos] samples=";
const ANCHOR: &str = "[rtos] anchor=";
const WORK: &str = "[bare] work=";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Case {
    Timing([bool; 4]), // Whether the service computes in each measured round.
    VsTimer,
    VsTimerMulti,
    VsTimerCohort,
    VsTimerSmp,
    ConsoleIrq { remote: bool },
    IrqLatency { scale: bool },
}
impl Case {
    pub(super) fn harts(self) -> u8 {
        match self {
            Self::VsTimerSmp | Self::ConsoleIrq { remote: true } | Self::IrqLatency { .. } => 2,
            _ => 1,
        }
    }

    pub(super) fn is_vs_timer(self) -> bool {
        matches!(
            self,
            Self::VsTimer | Self::VsTimerMulti | Self::VsTimerCohort | Self::VsTimerSmp
        )
    }
}

pub(super) fn verify(
    c: &mut Controller,
    case: Case,
    observe_idle: bool,
    samples: &mut Capture,
) -> Result<()> {
    let busy_rounds = match case {
        Case::Timing(rounds) => rounds,
        Case::VsTimer => return vtimer::verify(c),
        Case::VsTimerMulti => return vtimer::verify_multi(c),
        Case::VsTimerCohort => return vtimer::verify_cohort(c),
        Case::VsTimerSmp => return vtimer::verify_smp(c),
        Case::ConsoleIrq { remote } => return irq::verify(c, remote),
        Case::IrqLatency { scale } => return irq::latency::verify(c, scale, samples),
    };
    let control = c.status("control", "Ready")?;
    let old = c.status("service", "Ready")?;
    let src = Source::Guest(control);
    let hz = line(c, src, &mut 0, "[rtos] scheduler ready hz=")?.parse::<u32>()?;
    ensure!(hz != 0, "invalid RTOS timebase");
    timing::require_probe(c.log(src))?;
    c.wait(Source::Guest(old), 0, "[bare] ready")?;
    respond(c, "service", old, b'p', "[bare] pong")?;

    // The idle control uses the same epochs, serial handshakes and release windows
    // as ABBA. Only the two middle mode commands change.
    let mut score = Score::default();
    let mut ever_busy = false;
    let mut last_work = 0;
    for (round, busy) in busy_rounds.into_iter().enumerate() {
        let (anchor, end, work) = phase(c, control, old, busy, hz, round + 1, observe_idle)
            .with_context(|| format!("firmware pair round {}", round + 1))?;
        ever_busy |= busy;
        // A cumulative counter also covers the gaps between scored windows.
        ensure!(
            ever_busy || work == 0,
            "idle peer did work before its first busy command"
        );
        score.add(anchor, end)?;
        last_work = work;
    }
    c.attach("service", old)?;
    let before_stop = confirm_busy(c, old, last_work)?;
    c.detach()?;
    c.command("vm stop service")?;
    c.terminal(2, old, "Stopped")?;
    let stopped = fresh_window(c, control)?;
    c.command("vm start service")?;
    let fresh = c.status("service", "Ready")?;
    ensure!(fresh != old, "service cold boot reused its epoch");
    c.wait(Source::Guest(fresh), 0, "[bare] ready")?;
    c.attach("service", fresh)?;
    c.guest(fresh, b'p', "[bare] pong")?;
    ensure!(
        query_work(c, fresh)? == 0,
        "cold boot retained service work"
    );
    c.detach()?;
    let restarted = fresh_window(c, control)?;
    ensure!(
        score.last.samples < stopped
            && stopped < restarted
            && c.status("control", "Ready")? == control,
        "RTOS release sequence or epoch changed across peer restart"
    );
    println!(
        "firmware peer stop/cold restart and control continuity: PASS (work {before_stop} -> 0); timing observations: scored_releases={} missed_periods={} (not a timing SLA)",
        score.releases, score.missed
    );
    Ok(())
}

fn phase(
    c: &mut Controller,
    control: u32,
    service: u32,
    busy: bool,
    hz: u32,
    round: usize,
    observe_idle: bool,
) -> Result<(Boundary, Boundary, u64)> {
    let (key, mode) = if busy { (b'b', "busy") } else { (b'i', "idle") };
    c.attach("service", service)?;
    c.guest(service, key, &format!("[bare] {mode}"))?;
    let from = query_work(c, service)?;
    c.detach()?;
    park::control_waiting(c, control)?;
    let wakes = observe_idle.then(|| idle_snapshot(c)).transpose()?;

    // An RTOS report already in flight may predate the mode ACK. Request a
    // boundary from the RTOS itself, then consume its stream from that exact byte.
    let (anchor, mut cursor) = begin(c, control, b't')?;
    let (previous, maxima, bins) = timing::pair(c, control, &mut cursor, anchor, hz)?;
    c.detach()?;
    let idle_delta = wakes
        .map(|before| idle_snapshot(c)?.delta(before))
        .transpose()?;
    c.attach("service", service)?;
    let after = query_work(c, service)?;
    c.detach()?;
    let iterations = after
        .checked_sub(from)
        .context("service work counter regressed")?;
    ensure!(
        busy == (iterations != 0),
        "service did not exhibit requested {mode} work"
    );
    let peak = maxima[0].max(maxima[1]);
    println!(
        "firmware round={round} peer={mode} control_epoch={control} service_epoch={service} rtos_pre_window=Waiting samples={} window_max_ticks={maxima:?} max_late_us={} missed={} late_bins_2_10_20ms={bins:?} work_iterations={iterations} timebase_hz={hz}",
        previous.samples - anchor.samples,
        u128::from(peak) * 1_000_000 / u128::from(hz),
        previous.missed - anchor.missed
    );
    if let (Some(before), Some(delta)) = (wakes, idle_delta) {
        println!(
            "firmware round={round} HS WFI pending on return: {delta}; start_ticks={} {}",
            before.ticks(),
            delta.normalized(before.ticks(), hz)
        );
    }
    Ok((anchor, previous, after))
}

fn confirm_busy<T: Serial>(c: &mut Controller<T>, epoch: u32, previous: u64) -> Result<u64> {
    c.guest(epoch, b'b', "[bare] busy")?;
    // After acknowledging 'b', the service completes a batch before its next
    // UART poll. Even a queued '?' is causal: no sleep, retry or timing SLA.
    let current = query_work(c, epoch)?;
    ensure!(
        current > previous,
        "busy service did not advance before stop: {previous} -> {current}"
    );
    Ok(current)
}

fn query_work<T: Serial>(c: &mut Controller<T>, epoch: u32) -> Result<u64> {
    let source = Source::Guest(epoch);
    let mut cursor = c.log(source).len();
    c.guest(epoch, b'?', WORK)?;
    work(line(c, source, &mut cursor, WORK)?)
}
fn begin(c: &mut Controller, epoch: u32, key: u8) -> Result<(Boundary, usize)> {
    c.attach("control", epoch)?;
    let src = Source::Guest(epoch);
    let mut cursor = c.log(src).len();
    c.guest(epoch, key, ANCHOR)?;
    Ok((Boundary::parse(line(c, src, &mut cursor, ANCHOR)?)?, cursor))
}
fn window<T: Serial>(
    c: &mut Controller<T>,
    epoch: u32,
    cursor: &mut usize,
    anchor: Boundary,
) -> Result<Report> {
    let report = Report::parse(line(c, Source::Guest(epoch), cursor, REPORT)?)?;
    report.check_next(anchor)?;
    Ok(report)
}
fn fresh_window(c: &mut Controller, epoch: u32) -> Result<u64> {
    let (anchor, mut cursor) = begin(c, epoch, b's')?;
    let report = window(c, epoch, &mut cursor, anchor)?;
    c.detach()?;
    Ok(report.end.samples)
}
fn idle_snapshot(c: &mut Controller) -> Result<metrics::Wakes> {
    c.command("vm capacity")?;
    // The prompt has already arrived. A missing diagnostic is a build/config
    // mismatch, not a reason to spend the rest of the deadline waiting.
    let reply = std::str::from_utf8(&c.log(Source::Host)[c.last_command()..])?;
    metrics::Wakes::parse(
        reply
            .lines()
            .find_map(|l| l.strip_prefix("idle-wake "))
            .context("missing selftest idle-wake counters")?,
    )
}

// Return exactly one complete line, retaining coalesced later reports for the next call.
fn line<'a, T: Serial>(
    c: &'a mut Controller<T>,
    src: Source,
    cursor: &mut usize,
    prefix: &str,
) -> Result<&'a str> {
    c.wait(src, *cursor, prefix)?;
    let pos = *cursor
        + c.log(src)[*cursor..]
            .windows(prefix.len())
            .position(|w| w == prefix.as_bytes())
            .context("missing marker")?
        + prefix.len();
    c.wait(src, pos, "\n")?;
    let end = pos
        + c.log(src)[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .context("missing newline")?;
    *cursor = end + 1;
    Ok(std::str::from_utf8(&c.log(src)[pos..end])?.trim_end_matches('\r'))
}

#[cfg(test)]
mod tests;
