//! Two dynamically assigned volumes, isolated writes and explicit durable handoff.
//! Ownership/Lock Ordering: controller owns QEMU; both raw files belong to this scratch.
//! Invariants: no user disk, no network; a stopped/faulted VM retains its exclusive lease.
use super::{ALPHA, Controller, PONG, READY, Source, respond};
use crate::{
    config::{Input, ManagedDisk},
    media::Scratch,
};
use anyhow::{Result, ensure};

pub(super) fn media(scratch: &mut Scratch) -> Result<Vec<ManagedDisk>> {
    (0..2)
        .map(|index| {
            let (path, file) = scratch.file(&format!("volume-{index}.raw"))?;
            file.set_len(256 * 1024)?;
            Ok(ManagedDisk {
                input: Input::local(&path),
                writable: true,
            })
        })
        .collect()
}
fn row(c: &mut Controller, expected: &str) -> Result<()> {
    c.command("vm disks")?;
    c.wait(Source::Host, c.last_command(), expected)
}
fn start(c: &mut Controller, name: &str) -> Result<u32> {
    c.command(&format!("vm start {name}"))?;
    let epoch = c.status(name, "Ready")?;
    c.wait(Source::Guest(epoch), 0, READY)?;
    Ok(epoch)
}
fn stop(c: &mut Controller, id: u32, name: &str, epoch: u32) -> Result<()> {
    c.command(&format!("vm stop {name}"))?;
    c.terminal(id, epoch, "Stopped")
}
pub(super) fn verify(c: &mut Controller, parent: u32) -> Result<()> {
    row(c, "disk0 rw managed free")?;
    row(c, "disk1 rw managed free")?;
    c.command("vm create alpha left --disk 0")?;
    c.command("vm create beta right --disk 1")?;
    row(c, "disk0 rw managed VM 3")?;
    row(c, "disk1 rw managed VM 4")?;
    let left = start(c, "left")?;
    let right = start(c, "right")?;
    c.expect_busy("vm create alpha conflict --disk 0")?;
    respond(c, "left", left, b'w', "[probe] disk WRITE FLUSH PASS")?;
    respond(c, "right", right, b'z', "[probe] disk ZERO PASS")?;
    respond(c, "right", right, b'w', "[probe] disk WRITE FLUSH PASS")?;
    c.attach("left", left)?;
    c.guest(left, b'f', "[probe] touching unmapped guest page")?;
    c.terminal(3, left, "Faulted")?;
    c.expect_busy("vm create alpha conflict --disk 0")?;
    respond(c, "right", right, b'r', "[probe] disk READ PASS")?;
    let restarted = start(c, "left")?;
    ensure!(restarted != left, "cold restart reused identity");
    respond(c, "left", restarted, b'r', "[probe] disk READ PASS")?;
    stop(c, 3, "left", restarted)?;
    c.expect_busy("vm create alpha conflict --disk 0")?;
    c.destroy(3, "left")?;
    row(c, "disk0 rw managed free")?;
    c.command("vm create alpha successor --disk 0")?;
    let successor = start(c, "successor")?;
    ensure!(
        successor != left && successor != restarted,
        "recycled VM retained old authority"
    );
    respond(c, "successor", successor, b'r', "[probe] disk READ PASS")?;
    respond(c, ALPHA, parent, b'p', PONG)?;
    stop(c, 3, "successor", successor)?;
    c.destroy(3, "successor")?;
    stop(c, 4, "right", right)?;
    c.destroy(4, "right")?;
    row(c, "disk1 rw managed free")
}
