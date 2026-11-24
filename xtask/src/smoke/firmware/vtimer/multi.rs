//! Two independently measured busy peers and a timer probe sharing one native hart.
//! The controller owns each epoch's UART cursor; no guest or host locks are shared.
//! Worker: parked -> measuring -> report and park; stop may discard an open window.
use super::{Controller, Probe, Serial, Source, Visit, cpu, native_board, status};
use anyhow::{Context, Result, ensure};

const READY: &str = "[fair] ready\n";
const MEASURING: &[u8] = b"[fair] measuring\n";

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Window {
    batches: u64,
    first: u64,
    last: u64,
    max_gap: u64,
}

impl Window {
    fn parse(log: &[u8]) -> Result<Self> {
        let line = std::str::from_utf8(log)?
            .strip_suffix('\n')
            .context("unterminated fair-worker reply")?;
        let mut fields = line.split(' ');
        ensure!(fields.next() == Some("[fair]"), "not a fair-worker record");
        let mut field = |key| {
            let s = fields
                .next()
                .and_then(|f| f.strip_prefix(key))
                .with_context(|| format!("missing worker field {key}"))?;
            super::super::work(s)
        };
        let window = Self {
            batches: field("batches=")?,
            first: field("first=")?,
            last: field("last=")?,
            max_gap: field("max_gap=")?,
        };
        ensure!(fields.next().is_none(), "extra fair-worker fields");
        Ok(window)
    }

    /// Each adjacent pair of batch checkpoints is at most max_gap apart.
    /// A span crossing k strictly interior checkpoints fits in k+1 such gaps.
    pub(super) fn progress_inside(&self, (start, end): (u64, u64)) -> Result<u64> {
        ensure!(
            self.first <= start && start < end && end <= self.last,
            "worker window does not enclose scored timer visit: {self:?} vs {start}..{end}"
        );
        let span = self.last - self.first;
        ensure!(
            self.max_gap != 0
                && self.max_gap <= span
                && self.batches >= span.div_ceil(self.max_gap),
            "inconsistent worker checkpoint record: {self:?}"
        );
        let interior = (end - start).div_ceil(self.max_gap) - 1;
        ensure!(
            interior > 0,
            "insufficient evidence of worker progress inside scored visit: {self:?} vs {start}..{end}"
        );
        Ok(interior)
    }
}

pub(super) struct Worker {
    pub(super) name: &'static str,
    pub(super) epoch: u32,
    pub(super) owner: u8,
}

impl Worker {
    pub(super) fn ready(
        c: &mut Controller<impl Serial>,
        id: u32,
        name: &'static str,
        owner: u8,
    ) -> Result<Self> {
        let epoch = status(c, id, name, "32KiB", owner, false)?;
        let src = Source::Guest(epoch);
        c.wait(src, 0, READY)?;
        ensure!(
            c.log(src) == READY.as_bytes(),
            "unexpected {name} boot transcript"
        );
        Ok(Self { name, epoch, owner })
    }

    fn command<'a>(&self, c: &'a mut Controller<impl Serial>, key: u8) -> Result<&'a [u8]> {
        let src = Source::Guest(self.epoch);
        c.attach(self.name, self.epoch)?;
        let start = c.log(src).len();
        c.guest(self.epoch, key, "\n")?;
        c.detach()?;
        Ok(&c.log(src)[start..])
    }

    pub(super) fn begin(&self, c: &mut Controller<impl Serial>) -> Result<()> {
        ensure!(
            self.command(c, b'm')? == MEASURING,
            "missing worker measurement ACK"
        );
        Ok(())
    }

    pub(super) fn end(&self, c: &mut Controller<impl Serial>) -> Result<Window> {
        Window::parse(self.command(c, b'x')?)
    }
    pub(super) fn now(&self, c: &mut Controller<impl Serial>) -> Result<u64> {
        let stamp = std::str::from_utf8(self.command(c, b'n')?)?
            .strip_prefix("[fair] now=")
            .and_then(|s| s.strip_suffix('\n'))
            .context("invalid worker TIME reply")?;
        super::super::work(stamp)
    }

    pub(super) fn parked(&self, c: &mut Controller<impl Serial>) -> Result<()> {
        loop {
            ensure!(
                c.status(self.name, "Ready")? == self.epoch,
                "worker epoch changed"
            );
            // A guest can emit its ACK one exit before WFI. Poll only outside
            // the measured interval, under the suite's existing hard deadline.
            match cpu(&c.log(Source::Host)[c.last_command()..], self.owner)? {
                "Started retired=false run=Waiting" => return Ok(()),
                "Started retired=false run=Runnable" => {}
                other => anyhow::bail!("worker is not live: {other}"),
            }
        }
    }
}

fn round(
    c: &mut Controller<impl Serial>,
    probe: &mut Probe,
    peers: &[Worker; 2],
    busy: bool,
) -> Result<(Visit, Vec<(Window, u64)>)> {
    for peer in peers {
        if busy {
            peer.begin(c)?
        } else {
            peer.parked(c)?
        }
    }
    let visit = probe.capture(c)?;
    let mut windows = Vec::new();
    for peer in peers {
        let w = peer.end(c)?;
        if busy {
            let floor = w
                .progress_inside(visit.scored_window())
                .with_context(|| peer.name)?;
            windows.push((w, floor));
        } else {
            ensure!(w == Window::default(), "parked {} reported work", peer.name);
        }
    }
    Ok((visit, windows))
}

fn restart(c: &mut Controller<impl Serial>, probe: &mut Probe, peers: &[Worker; 2]) -> Result<()> {
    let [source, copy] = peers;
    source.begin(c)?;
    copy.begin(c)?;
    c.command("vm stop peer2")?;
    c.terminal(3, copy.epoch, "Stopped")?;
    let stopped = probe.capture(c)?;
    c.command("vm start peer2")?;
    let fresh = Worker::ready(c, 3, "peer2", 0)?;
    ensure!(
        ![source.epoch, copy.epoch, probe.epoch].contains(&fresh.epoch),
        "reused worker epoch"
    );
    ensure!(
        fresh.end(c)? == Window::default(),
        "cold clone retained an open measurement"
    );
    let booted = probe.capture(c)?;
    let live = source.end(c)?;
    live.progress_inside(stopped.scored_window())?;
    live.progress_inside(booted.scored_window())?;
    c.command("vm stop peer2")?;
    c.terminal(3, fresh.epoch, "Stopped")?;
    c.destroy(3, "peer2")?;
    ensure!(
        status(c, 2, "service", "32KiB", 0, false)? == source.epoch,
        "source restarted with clone"
    );
    probe.shutdown(c)?;
    println!(
        "multiway: peer2 cold restart {}->{}, source epoch={} and timer epoch={} continued",
        copy.epoch, fresh.epoch, source.epoch, probe.epoch
    );
    Ok(())
}

pub(in crate::smoke::firmware) fn verify_multi(c: &mut Controller<impl Serial>) -> Result<()> {
    let (_, hz) = native_board(c.log(Source::Host), 1)?;
    ensure!(hz == 10_000_000, "timer probe expects 10 MHz TIME");
    let mut probe = Probe::start(c)?;
    let source = Worker::ready(c, 2, "service", 0)?;
    c.command("vm create service peer2")?;
    c.wait(
        Source::Host,
        c.last_command(),
        "VM 3 created: peer2 (stopped)\n",
    )?;
    c.command("vm start peer2")?;
    let copy = Worker::ready(c, 3, "peer2", 0)?;
    ensure!(
        source.epoch != probe.epoch && ![source.epoch, probe.epoch].contains(&copy.epoch),
        "multiway guests share an epoch"
    );
    let peers = [source, copy];
    let mut rounds = Vec::new();
    for busy in [false, true, true, false] {
        rounds.push(round(c, &mut probe, &peers, busy)?);
    }
    restart(c, &mut probe, &peers)?;
    for (index, (visit, windows)) in rounds.iter().enumerate() {
        let n = index + 1;
        let mode = if windows.is_empty() {
            "two-parked"
        } else {
            "two-busy"
        };
        visit.report(probe.epoch, n, mode)?;
        let (start, end) = visit.scored_window();
        for (worker, (w, floor)) in peers.iter().zip(windows) {
            println!(
                "multiway round={n} peer={} epoch={} scored_ticks={start}..{end} worker_ticks={}..{} batches={} max_batch_gap_ticks={} scored_min_checkpoints={floor}",
                worker.name, worker.epoch, w.first, w.last, w.batches, w.max_gap
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
