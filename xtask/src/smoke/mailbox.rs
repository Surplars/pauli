//! Validate one two-vCPU VS mailbox benchmark, not Linux or physical IPI latency.
//! Only the selected guest epoch supplies samples; host status proves placement and join.
//! Both guest command handshakes and all terminal checks are outside the timed intervals.
use super::{ALPHA, Controller, Source, controller::native_board};
use anyhow::{Context, Result, ensure};
mod handoff;
mod split;

const OPS: [&str; 3] = ["ipi", "vma", "fencei"];
const N: usize = 24;
const WARMUP: usize = 4;
const READY: &str = "[mb] ready cpus=2 hz=";
const DONE: &str = "[mb] done\n";
const OFF: &str = "[mb] poweroff\n";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Workload {
    RoundTrip,
    FenceSplit,
    IpiHandoff,
}
impl Workload {
    fn key(self) -> u8 {
        match self {
            Self::RoundTrip => b'g',
            Self::FenceSplit => b'd',
            Self::IpiHandoff => b'i',
        }
    }
}

pub(super) fn verify(
    c: &mut Controller<impl super::controller::Serial>,
    host_cpus: u8,
    workload: Workload,
) -> Result<()> {
    let epoch = c.status(ALPHA, "Ready")?;
    let src = Source::Guest(epoch);
    c.wait(src, 0, "\n")?;
    let (boot, hz) = native_board(c.log(Source::Host), host_cpus)?;
    let ready = format!("{READY}{hz}\n");
    ensure!(
        c.log(src) == ready.as_bytes(),
        "unexpected mailbox ready transcript"
    );

    // The first status can precede HSM start. Take a fresh snapshot only once
    // the guest's ready record proves that its worker has checked in.
    ensure!(
        c.status(ALPHA, "Ready")? == epoch,
        "mailbox guest restarted"
    );
    // The requested physical pins do not rotate when OpenSBI boots on hart 1.
    let pins = [0, host_cpus - 1];
    owners(&c.log(Source::Host)[c.last_command()..], epoch, false, pins)?;
    c.attach(ALPHA, epoch)?;
    c.guest(epoch, workload.key(), DONE)?;
    let end = c.log(src).len();
    c.guest(epoch, b'q', OFF)?;
    c.terminal(1, epoch, "Stopped")?;
    ensure!(
        c.status(ALPHA, "Stopped")? == epoch,
        "wrong joined mailbox epoch"
    );
    let status = &c.log(Source::Host)[c.last_command()..];
    owners(status, epoch, true, pins)?;
    ensure!(
        c.log(src).get(end..) == Some(OFF.as_bytes()),
        "unexpected mailbox shutdown transcript"
    );
    // Score only the selected epoch's measurement prefix, after proof of join.
    let log = &c.log(src)[..end];
    if workload == Workload::IpiHandoff {
        return handoff::parse_and_report(log, &ready, epoch, hz, pins);
    }
    let split = workload == Workload::FenceSplit;
    let samples = parse(log, &ready, split)?;
    if split {
        // Only the new native reply is evidence; old output and guest frames cannot qualify.
        c.command(&format!("vm fences {ALPHA}"))?;
        let reply = &c.log(Source::Host)[c.last_command()..];
        split::report(reply, epoch, pins[1], &samples)?;
    }
    println!(
        "mailbox guest: epoch={epoch} boot={boot} owners={pins:?} receipts={} host_cpus={host_cpus}",
        N + WARMUP
    );
    for (op, windows) in OPS.into_iter().zip(samples) {
        let raw = windows.map(|w| w.ticks);
        let [p50, p95, max] = ranks(raw);
        let us = |v: u64| v as f64 * 1e6 / hz as f64;
        println!(
            "mailbox op={op} n={N} timebase_hz={hz} p50_ticks={p50} p95_ticks={p95} max_ticks={max} p50_us={:.3} p95_us={:.3} max_us={:.3}",
            us(p50),
            us(p95),
            us(max)
        );
        println!("  ticks={raw:?}");
    }
    Ok(())
}

fn owners(reply: &[u8], epoch: u32, joined: bool, expected: [u8; 2]) -> Result<()> {
    let text = std::str::from_utf8(reply)?;
    let state = if joined { "Stopped" } else { "Ready" };
    let header = format!("VM id=1 epoch={epoch} name={ALPHA} state={state} vcpus=2 memory=");
    let mut headers = text.lines().filter(|r| r.starts_with("VM id="));
    ensure!(
        headers.next().is_some_and(|r| r.starts_with(&header)) && headers.next().is_none(),
        "expected one two-vCPU status for mailbox epoch={epoch}"
    );
    if joined {
        let mut exits = text.lines().filter(|r| r.starts_with("VM 1 exit="));
        ensure!(
            exits
                .next()
                .is_some_and(|r| r.starts_with("VM 1 exit=Shutdown "))
                && exits.next().is_none(),
            "mailbox guest did not shut down cleanly"
        );
    }
    let mut rows = text
        .lines()
        .filter_map(|r| r.trim_start().strip_prefix("vcpu="));
    for (i, pin) in expected.into_iter().enumerate() {
        let (cpu, tail) = rows
            .next()
            .context("missing mailbox vCPU")?
            .split_once(" owner=")
            .context("missing physical owner")?;
        let (owner, s) = tail.split_once(" state=").context("missing vCPU state")?;
        // The worker may still own its first slice when the host takes this
        // snapshot. ready proves it has entered; HSM state publishes at slice exit.
        let live = matches!(
            s,
            "Started retired=false run=Runnable" | "Started retired=false run=Waiting"
        ) || (i == 1 && s == "StartPending retired=false run=Starting");
        ensure!(
            cpu.parse::<usize>()? == i
                && owner.parse::<u8>()? == pin
                && (if joined {
                    s == "Stopped retired=true run=Stopped"
                } else {
                    live
                }),
            "incorrect mailbox vCPU{i} status: {tail}"
        );
    }
    ensure!(rows.next().is_none(), "unexpected third mailbox vCPU");
    Ok(())
}

#[derive(Default, Clone, Copy)]
struct Window {
    start: u64, // Only present on the diagnostic wire.
    ticks: u64,
}
fn decimal(s: &str) -> Result<u64> {
    ensure!(
        s.bytes().all(|b| b.is_ascii_digit()),
        "invalid decimal field"
    );
    Ok(s.parse()?)
}
fn parse(log: &[u8], ready: &str, split: bool) -> Result<[[Window; N]; 3]> {
    let text = std::str::from_utf8(log)?
        .strip_prefix(ready)
        .context("missing mailbox ready")?;
    let mut lines = text
        .strip_suffix(DONE)
        .and_then(|r| r.strip_suffix('\n'))
        .context("incomplete mailbox transcript")?
        .split('\n');
    let mut used = [0; 3];
    let mut data = [[Window::default(); N]; 3];
    let mut last_end = 0;
    for record in 0..N * OPS.len() {
        let row = lines
            .next()
            .context("missing mailbox sample")?
            .strip_prefix("[mb] sample op=")
            .context("unexpected mailbox record")?;
        let (op, row) = row
            .split_once(" seq=")
            .context("missing mailbox sample sequence")?;
        let (seq, ticks) = row
            .split_once(" ticks=")
            .context("missing mailbox sample ticks")?;
        let i = OPS
            .iter()
            .position(|&s| s == op)
            .context("unknown mailbox operation")?;
        ensure!(
            used[i] < N
                && seq.bytes().all(|b| b.is_ascii_digit())
                && seq.parse::<usize>()? == used[i] + 1,
            "missing, duplicate or late {op} sample"
        );
        let (ticks, start) = if split {
            let (ticks, begin) = ticks.split_once(" start=").context("missing guest start")?;
            let start = decimal(begin)?;
            let round = WARMUP + record / OPS.len();
            ensure!(
                i == (round + record % OPS.len()) % OPS.len() && start >= last_end,
                "wrong guest operation order or overlapping windows"
            );
            (decimal(ticks)?, start)
        } else {
            (decimal(ticks)?, 0)
        };
        last_end = start.checked_add(ticks).context("guest TIME overflow")?;
        data[i][used[i]] = Window { start, ticks };
        used[i] += 1;
    }
    // The remote worker increments receipts only after observing a real VS SSIP.
    let receipts = format!("[mb] receipts ipi={}", N + WARMUP);
    ensure!(
        used == [N; 3] && lines.next() == Some(receipts.as_str()) && lines.next().is_none(),
        "wrong mailbox operation count or remote IPI receipts"
    );
    Ok(data)
}

/// Nearest-rank P50/P95 and observed maximum; 24 samples make P95 the 23rd.
fn ranks(mut v: [u64; N]) -> [u64; 3] {
    v.sort_unstable();
    [
        v[N.div_ceil(2) - 1],
        v[(N * 95).div_ceil(100) - 1],
        v[N - 1],
    ]
}

#[cfg(test)]
mod tests;
