//! Measure the guest timer vector itself, not the release of an RTOS task.
//! The controller owns the serial cursor and one probe epoch. Mode changes,
//! work queries and UART reports are outside the 24 serialized timer windows.
use super::{Controller, Serial, Source, irq::cpu, query_work};
use crate::smoke::controller::native_board;
use anyhow::{Context, Result, ensure};
mod cohort;
mod multi;
mod records;
mod smp;
pub(super) use cohort::verify_cohort;
pub(super) use multi::verify_multi;
use records::{EVENTS, Visit};
pub(super) use smp::verify_smp;

const READY: &str = "[vt] ready\n";
const DONE: &str = "[vt] done\n";
const OFF: &str = "[vt] poweroff\n";

struct Probe {
    id: u32,
    name: &'static str,
    epoch: u32,
    owner: u8,
    next_gen: u64,
    cursor: usize,
    last_trap: u64,
}

impl Probe {
    fn start(c: &mut Controller<impl Serial>) -> Result<Self> {
        Self::start_vm(c, 1, "control", 0)
    }
    fn start_vm(
        c: &mut Controller<impl Serial>,
        id: u32,
        name: &'static str,
        owner: u8,
    ) -> Result<Self> {
        let epoch = status(c, id, name, "256KiB", owner, false)?;
        let src = Source::Guest(epoch);
        c.wait(src, 0, READY)?;
        ensure!(
            c.log(src) == READY.as_bytes(),
            "unexpected timer probe boot transcript"
        );
        Ok(Self {
            id,
            name,
            epoch,
            owner,
            next_gen: 1,
            cursor: READY.len(),
            last_trap: 0,
        })
    }
    fn capture(&mut self, c: &mut Controller<impl Serial>) -> Result<Visit> {
        let src = Source::Guest(self.epoch);
        c.attach(self.name, self.epoch)?;
        ensure!(
            c.log(src).len() == self.cursor,
            "unsolicited timer probe output"
        );
        c.guest(self.epoch, b't', DONE)?;
        c.detach()?;
        let log = c.log(src);
        let visit = records::parse(&log[self.cursor..], self.next_gen, self.last_trap)?;
        self.cursor = log.len();
        self.next_gen += EVENTS as u64;
        self.last_trap = visit.last_trap();
        Ok(visit)
    }
    fn shutdown(&self, c: &mut Controller<impl Serial>) -> Result<()> {
        c.attach(self.name, self.epoch)?;
        c.guest(self.epoch, b'q', OFF)?;
        c.terminal(self.id, self.epoch, "Stopped")?;
        ensure!(
            status(c, self.id, self.name, "256KiB", self.owner, true)? == self.epoch,
            "wrong joined timer epoch"
        );
        ensure!(
            c.log(Source::Guest(self.epoch)).get(self.cursor..) == Some(OFF.as_bytes()),
            "unexpected timer probe shutdown transcript"
        );
        Ok(())
    }
}

fn status(
    c: &mut Controller<impl Serial>,
    id: u32,
    name: &str,
    ram: &str,
    owner: u8,
    stopped: bool,
) -> Result<u32> {
    let state = if stopped { "Stopped" } else { "Ready" };
    let epoch = c.status(name, state)?;
    let reply = &c.log(Source::Host)[c.last_command()..];
    let text = std::str::from_utf8(reply)?;
    let header = format!("VM id={id} epoch={epoch} name={name} state={state} vcpus=1 memory={ram}");
    ensure!(
        text.lines()
            .filter(|l| l.starts_with("VM id="))
            .eq([&*header]),
        "wrong timer workload VM: {name}"
    );
    // A first host snapshot can precede first entry; the guest ready record
    // then proves execution. At shutdown require actual retirement and reason.
    let lane = cpu(reply, owner)?;
    if stopped {
        let mut exits = text
            .lines()
            .filter(|l| l.starts_with("VM ") && l.contains(" exit="));
        ensure!(
            lane == "Stopped retired=true run=Stopped"
                && exits
                    .next()
                    .is_some_and(|l| l.starts_with(&format!("VM {id} exit=Shutdown cause=")))
                && exits.next().is_none(),
            "timer probe did not retire after guest shutdown"
        );
    } else {
        ensure!(
            matches!(
                lane,
                "Started retired=true run=Runnable"
                    | "Started retired=false run=Runnable"
                    | "Started retired=false run=Waiting"
            ),
            "unexpected live {name} state: {lane}"
        );
    }
    Ok(epoch)
}

struct Round {
    visit: Visit,
    mode: &'static str,
    work: u64,
}

fn round(
    c: &mut Controller<impl Serial>,
    probe: &mut Probe,
    peer: u32,
    busy: bool,
    last: &mut u64,
) -> Result<Round> {
    let (key, mode) = if busy { (b'b', "busy") } else { (b'i', "idle") };
    c.attach("service", peer)?;
    c.guest(peer, key, &format!("[bare] {mode}\n"))?;
    let before = query_work(c, peer)?;
    ensure!(before >= *last, "peer work regressed before timer visit");
    c.detach()?;
    let visit = probe.capture(c)?;
    c.attach("service", peer)?;
    let after = query_work(c, peer)?;
    c.detach()?;
    let work = after
        .checked_sub(before)
        .context("peer work regressed during timer visit")?;
    ensure!(busy == (work != 0), "peer work does not match {mode} mode");
    *last = after;
    Ok(Round { visit, mode, work })
}

pub(super) fn verify(c: &mut Controller<impl Serial>) -> Result<()> {
    let (_, hz) = native_board(c.log(Source::Host), 1)?;
    ensure!(
        hz == 10_000_000,
        "timer probe expects 10 MHz TIME, got {hz}"
    );
    let mut probe = Probe::start(c)?;
    let peer = status(c, 2, "service", "32KiB", 0, false)?;
    ensure!(probe.epoch != peer, "firmware pair reused an epoch");
    c.wait(Source::Guest(peer), 0, "[bare] ready\n")?;
    let mut rounds = Vec::new();
    let mut last = 0;
    for busy in [false, true, true, false] {
        rounds.push(round(c, &mut probe, peer, busy, &mut last)?);
    }
    probe.shutdown(c)?;

    // This visit is a lifecycle gate, never a fifth ABBA performance sample.
    c.command("vm start control")?;
    let mut fresh = Probe::start(c)?;
    ensure!(
        fresh.epoch != probe.epoch && fresh.epoch != peer,
        "timer probe reused epoch"
    );
    fresh.capture(c)?;
    fresh.shutdown(c)?;
    ensure!(
        status(c, 2, "service", "32KiB", 0, false)? == peer,
        "peer restarted with timer probe"
    );
    c.attach("service", peer)?;
    ensure!(
        last != 0 && query_work(c, peer)? == last,
        "idle peer lost its work across timer restart"
    );
    c.detach()?;
    for (index, r) in rounds.iter().enumerate() {
        println!(
            "vs-timer peer_epoch={peer} round={} mode={} work_iterations={}",
            index + 1,
            r.mode,
            r.work
        );
        r.visit.report(probe.epoch, index + 1, r.mode)?;
    }
    println!(
        "vs-timer cold restart {}->{}: {} fresh receipts, peer epoch={peer} preserved",
        probe.epoch, fresh.epoch, EVENTS
    );
    Ok(())
}

#[cfg(test)]
mod replay;
