//! Evidence parser for single-byte serial frames and bounded, noninteractive commands.
//! Ownership/Lock Ordering: sole controller owns child and transcript; no child input on drop.
//! Invariants: never accept guest bytes as host proof or a previous epoch as a fresh boot.
use crate::process::{Output, Session};
use anyhow::{Context, Result, bail, ensure};
use pauli_devices::console::{Decoder, Event, Source};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
const LIMIT: usize = 1024 * 1024;
const HOST: Source = Source::Host;
const PROMPT: &str = "hv> ";

/// A successful pipe write is not a UART acknowledgement.
pub(super) trait Serial {
    fn send(&mut self, bytes: &[u8]) -> Result<()>;
    fn receive(&mut self, timeout: Duration) -> Result<Output>;
}
impl Serial for Session {
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        Session::send(self, bytes)
    }
    fn receive(&mut self, timeout: Duration) -> Result<Output> {
        self.data(timeout)
    }
}
pub(super) struct Controller<T = Session> {
    process: T,
    deadline: Instant,
    decoder: Decoder,
    logs: BTreeMap<Source, Vec<u8>>,
    bytes: usize,
    last_host: usize,
    attached: Option<u32>,
    detach_cursor: Option<usize>,
}
impl<T: Serial> Controller<T> {
    #[cfg(test)]
    pub fn new(process: T, timeout: Duration) -> Self {
        Self::with_deadline(process, Instant::now() + timeout)
    }
    pub(super) fn with_deadline(process: T, deadline: Instant) -> Self {
        Self {
            process,
            deadline,
            decoder: Decoder::new(),
            logs: BTreeMap::new(),
            bytes: 0,
            last_host: 0,
            attached: None,
            detach_cursor: None,
        }
    }
    fn read(&mut self) -> Result<bool> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        ensure!(!left.is_zero(), "QEMU exceeded the hard deadline");
        match self.process.receive(left)? {
            Output::Data(bytes) => {
                self.ingest(&bytes)?;
                Ok(true)
            }
            Output::Idle => bail!("QEMU exceeded the hard deadline"),
            Output::End => {
                ensure!(self.decoder.complete(), "truncated serial frame");
                Ok(false)
            }
        }
    }
    fn ingest(&mut self, bytes: &[u8]) -> Result<()> {
        self.bytes += bytes.len();
        ensure!(self.bytes <= LIMIT, "serial capture exceeded 1 MiB");
        let mut run = None;
        for &byte in bytes {
            match self
                .decoder
                .push(byte)
                .map_err(|_| anyhow::anyhow!("bad serial framing"))?
            {
                Some(Event::Byte(source, byte)) => {
                    if run.is_none_or(|(active, _)| active != source) {
                        self.check_run(run)?;
                        run = Some((source, self.log(source).len()));
                    }
                    self.logs.entry(source).or_default().push(byte);
                }
                Some(Event::Attach(epoch)) => {
                    self.attached = Some(epoch);
                    self.detach_cursor = None;
                }
                Some(Event::Detach) => {
                    self.attached = None;
                    self.detach_cursor = Some(self.log(HOST).len());
                }
                _ => {}
            }
        }
        self.check_run(run)
    }
    /// Only sources appended in this chunk can complete a new failure marker.
    fn check_run(&self, run: Option<(Source, usize)>) -> Result<()> {
        let Some((source, old_len)) = run else {
            return Ok(());
        };
        let found = |marker: &[u8]| {
            contains(
                scan_window(self.log(source), 0, old_len, marker.len()),
                marker,
            )
        };
        if source == HOST {
            ensure!(
                !found(b"[HV] ERROR")
                    && !found(b"provisioning failed")
                    && !found(b"I/O join error="),
                "hypervisor reported a failure"
            );
        } else {
            ensure!(
                !found(b"[probe] FAIL"),
                "Linux smoke fixture reported a failure"
            );
            ensure!(!found(b"[irq] FAIL\n"), "IRQ firmware reported a failure");
            ensure!(
                !found(b"[vt] FAIL\n"),
                "VS timer firmware reported a failure"
            );
            ensure!(
                !found(b"[mb] FAIL"),
                "mailbox benchmark guest reported a failure"
            );
            ensure!(
                !found(b"[lat] FAIL\n"),
                "IRQ latency firmware reported a failure"
            );
        }
        Ok(())
    }
    pub(super) fn log(&self, source: Source) -> &[u8] {
        self.logs.get(&source).map(Vec::as_slice).unwrap_or(&[])
    }
    /// Unframed native probes own this UART. Keep the cursor across fragmented
    /// reads so EOF, including any record after a completion marker, is checked.
    pub(super) fn native_line(&mut self, cursor: &mut usize) -> Result<Option<&[u8]>> {
        let mut seen = *cursor;
        loop {
            let log = self.log(HOST);
            if let Some(offset) = log[seen..].iter().position(|&b| b == b'\n') {
                let end = seen + offset;
                let line = &self.log(HOST)[*cursor..end];
                *cursor = end + 1;
                return Ok(Some(line.strip_suffix(b"\r").unwrap_or(line)));
            }
            seen = log.len();
            if !self.read()? {
                ensure!(*cursor == seen, "unterminated native UART record");
                return Ok(None);
            }
        }
    }
    fn until(&mut self, source: Source, start: usize, marker: &[u8]) -> Result<()> {
        let mut seen = start;
        loop {
            let log = self.log(source);
            if contains(scan_window(log, start, seen, marker.len()), marker) {
                return Ok(());
            }
            seen = log.len();
            ensure!(
                self.read()?,
                "QEMU exited before the expected serial output"
            );
        }
    }
    pub fn wait(&mut self, source: Source, start: usize, marker: &str) -> Result<()> {
        self.until(source, start, marker.as_bytes())
            .with_context(|| format!("waiting for {source:?}: {marker}"))
    }
    pub fn report_failure(self) {
        // Reap before writing logs: the host's diagnostic sink may itself block.
        drop(self.process);
        for (source, bytes) in &self.logs {
            eprintln!(
                "{source:?} tail:\n{}",
                String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(8192)..])
            );
        }
    }
    /// Windows QEMU stdio can discard input while its UART cannot receive.
    /// The monitor echoes every printable key after draining it. Keep one in
    /// flight; send LF only after the last echo, then look for a fresh reply.
    fn host_line(&mut self, command: &str) -> Result<usize> {
        ensure!(
            self.attached.is_none(),
            "expected management console for {command}"
        );
        for (i, byte) in command.bytes().enumerate() {
            let start = self.log(HOST).len();
            self.process.send(&[byte])?;
            self.until(HOST, start, &[byte]).with_context(|| {
                format!(
                    "host command {command:?}: waiting for echo {}/{} ({:?})",
                    i + 1,
                    command.len(),
                    char::from(byte)
                )
            })?;
        }
        let start = self.log(HOST).len();
        self.process.send(b"\n")?;
        Ok(start)
    }
    pub fn shutdown(&mut self) -> Result<()> {
        self.host_line("shutdown").map(|_| ())
    }
    pub fn command(&mut self, command: &str) -> Result<()> {
        let reply = self.command_reply(command)?;
        ensure!(!contains(reply, b"vm: "), "host command failed: {command}");
        Ok(())
    }
    pub fn expect_busy(&mut self, command: &str) -> Result<()> {
        let reply = self.command_reply(command)?;
        ensure!(
            contains(reply, b"vm: Busy\n"),
            "host did not reject busy resource: {command}"
        );
        Ok(())
    }
    fn command_reply(&mut self, command: &str) -> Result<&[u8]> {
        self.last_host = self.host_line(command)?;
        self.wait(HOST, self.last_host, PROMPT)
            .with_context(|| format!("host command {command:?} did not return to the prompt"))?;
        Ok(&self.log(HOST)[self.last_host..])
    }
    pub fn last_command(&self) -> usize {
        self.last_host
    }
    pub fn destroy(&mut self, id: u32, vm: &str) -> Result<()> {
        let command = format!("vm destroy {vm}");
        let success = format!("VM {id} destroyed\n");
        loop {
            let start = self.host_line(&command)?;
            self.wait(HOST, start, PROMPT)?;
            let reply = &self.log(HOST)[start..];
            if contains(reply, b"vm: Busy\n") {
                continue;
            } // Transient owner pin; same hard deadline.
            ensure!(
                contains(reply, success.as_bytes()),
                "destroy failed for {vm}"
            );
            return Ok(());
        }
    }
    pub fn status(&mut self, vm: &str, state: &str) -> Result<u32> {
        self.command(&format!("vm status {vm}"))?;
        status_epoch(&self.log(HOST)[self.last_host..], vm, state)
    }
    pub fn attach(&mut self, vm: &str, epoch: u32) -> Result<()> {
        self.host_line(&format!("vm attach {vm}"))?;
        while self.attached.is_none() {
            ensure!(
                self.read()
                    .with_context(|| format!("waiting to attach {vm} epoch={epoch}"))?,
                "QEMU exited before console attach"
            );
        }
        ensure!(
            self.attached == Some(epoch),
            "host attached a different epoch"
        );
        Ok(())
    }
    pub fn detach(&mut self) -> Result<()> {
        ensure!(self.attached.is_some(), "no attached guest");
        let cursor = self.log(HOST).len();
        self.process.send(b"\x1d")?;
        self.wait(HOST, cursor, PROMPT)?;
        ensure!(self.attached.is_none(), "console did not detach");
        Ok(())
    }
    pub fn guest(&mut self, epoch: u32, key: u8, marker: &str) -> Result<()> {
        ensure!(self.attached == Some(epoch), "send to wrong console");
        let source = Source::Guest(epoch);
        let start = self.log(source).len();
        self.process.send(&[key])?;
        self.wait(source, start, marker)
    }
    pub fn terminal(&mut self, id: u32, epoch: u32, state: &str) -> Result<()> {
        self.wait(HOST, 0, &format!("VM {id} epoch={epoch} {state}\n"))?;
        while self.attached == Some(epoch) {
            ensure!(self.read()?, "QEMU exited before terminal detach");
        }
        // A detach frame and its prompt may arrive in different chunks.
        if let Some(cursor) = self.detach_cursor {
            self.wait(HOST, cursor, PROMPT)?;
        }
        Ok(())
    }
}
impl Controller<Session> {
    pub fn finish(&mut self) -> Result<()> {
        while self.read()? {}
        self.process.finish(Some(self.deadline))
    }
}
/// Keep only possible matches ending in new bytes, never before the caller's freshness cursor.
fn scan_window(bytes: &[u8], start: usize, seen: usize, width: usize) -> &[u8] {
    if seen == bytes.len() {
        return &[];
    }
    &bytes[start.max(seen.saturating_sub(width - 1))..]
}
fn contains(bytes: &[u8], pattern: &[u8]) -> bool {
    bytes.windows(pattern.len()).any(|window| window == pattern)
}
/// The native banner, not the workload or guest log, attests topology and TIME rate.
pub(super) fn native_board(log: &[u8], host_cpus: u8) -> Result<(u8, u64)> {
    let mut banners = std::str::from_utf8(log)?
        .lines()
        .filter_map(|line| line.strip_prefix("[HV] QEMU virt: "));
    let (harts, rest) = banners
        .next()
        .context("missing native board banner")?
        .split_once(" harts, boot ")
        .context("invalid native topology")?;
    let (boot, hz) = rest.split_once(", ").context("invalid native topology")?;
    let boot: u8 = boot.parse()?;
    let hz: u64 = hz
        .strip_suffix(" Hz")
        .context("missing native timebase")?
        .parse()?;
    ensure!(
        harts.parse::<u8>()? == host_cpus
            && boot < host_cpus
            && hz != 0
            && banners.next().is_none(),
        "ambiguous topology or timebase"
    );
    Ok((boot, hz))
}

fn status_epoch(bytes: &[u8], vm: &str, state: &str) -> Result<u32> {
    let text = std::str::from_utf8(bytes).context("invalid host text")?;
    let prefix = format!("name={vm} state={state} ");
    for line in text.lines() {
        let Some((head, tail)) = line.split_once(" epoch=") else {
            continue;
        };
        if !head.starts_with("VM id=") {
            continue;
        }
        let Some((epoch, fields)) = tail.split_once(' ') else {
            continue;
        };
        if fields.starts_with(&prefix) {
            return epoch.parse().context("invalid status epoch");
        }
    }
    bail!("status for {vm} was not {state}: {text}")
}
#[test]
fn status_requires_an_exact_native_state_line() {
    let status = b"vm status alpha\nVM 1 started\nVM id=1 epoch=7 name=alpha state=Ready vcpus=2 memory=96MiB\nhv> ";
    assert_eq!(status_epoch(status, "alpha", "Ready").unwrap(), 7);
    assert!(status_epoch(status, "alpha", "Stopped").is_err());
    assert!(status_epoch(b"VM 1 started", "alpha", "Ready").is_err());
    assert!(status_epoch(status, "alp", "Ready").is_err());
}

#[cfg(test)]
mod tests;
