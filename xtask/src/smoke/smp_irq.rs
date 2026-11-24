//! Linux RTC routing on two guest CPUs. Only the addressed epoch supplies IRQ counts.
//! Joined host state separately attests hardware file use; guest DT and build flags cannot.
use super::{ALPHA, Controller, READY, Source};
use anyhow::{Context, Result, ensure};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum Backend {
    Plic,
    SoftwareImsic,
    NativeImsic,
}
impl Backend {
    pub(super) fn new(aia: bool, native: bool) -> Self {
        if native {
            Self::NativeImsic
        } else if aia {
            Self::SoftwareImsic
        } else {
            Self::Plic
        }
    }
}

pub(super) fn verify(c: &mut Controller, backend: Backend) -> Result<()> {
    let mut old = None;
    for _ in 0..2 {
        if old.is_some() {
            c.command("vm start alpha")?;
        }
        let epoch = c.status(ALPHA, "Ready")?;
        ensure!(old != Some(epoch), "RTC smoke reused the old VM epoch");
        let guest = Source::Guest(epoch);
        c.wait(guest, 0, "[probe] SMP HSM affinity PASS")?;
        c.wait(guest, 0, READY)?;
        let [one, zero] = reports(c.log(guest))?;
        c.attach(ALPHA, epoch)?;
        c.guest(epoch, b'q', "[probe] Poweroff")?;
        c.terminal(1, epoch, "Stopped")?;
        ensure!(c.status(ALPHA, "Stopped")? == epoch, "wrong joined epoch");
        let reply = std::str::from_utf8(&c.log(Source::Host)[c.last_command()..])?;
        ensure!(
            reply.lines().any(|l| l.starts_with("VM 1 exit=Shutdown ")),
            "not a clean shutdown"
        );
        let owners = retired_owners(reply)?;
        let file = files(reply, epoch, backend)?;
        println!(
            "Linux RTC {backend:?}: epoch={epoch} owners={owners:?} irq={} CPU1 {:?}->{:?}, CPU0 {:?}->{:?}; {file}",
            one.irq, one.before, one.after, zero.before, zero.after
        );
        old = Some(epoch);
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct Report {
    irq: u32,
    before: [u64; 2],
    after: [u64; 2],
}
fn reports(log: &[u8]) -> Result<[Report; 2]> {
    // Ignore unrelated kernel printk bytes; parse only complete PID1 records for this VM.
    let mut rows = log
        .split_inclusive(|&b| b == b'\n')
        .filter_map(|l| l.strip_prefix(b"[probe] RTC IRQ irq="));
    let mut report = |cpu: usize| -> Result<Report> {
        let row = std::str::from_utf8(rows.next().context("missing RTC IRQ report")?)?;
        let row = row
            .strip_suffix('\n')
            .context("partial RTC IRQ report")?
            .trim_end_matches('\r');
        let (irq, rest) = row.split_once(" cpu=").context("missing RTC IRQ")?;
        let (target, rest) = rest.split_once(" before=").context("missing IRQ target")?;
        let (a, b) = rest.split_once(" after=").context("missing IRQ counters")?;
        let counts = |s: &str| -> Result<[u64; 2]> {
            let (l, r) = s.split_once(',').context("expected two CPU counters")?;
            Ok([l.parse()?, r.parse()?])
        };
        let r = Report {
            irq: irq.parse()?,
            before: counts(a)?,
            after: counts(b)?,
        };
        ensure!(
            r.irq != 0 && target.parse::<usize>()? == cpu,
            "wrong RTC IRQ target"
        );
        ensure!(
            r.after[cpu].checked_sub(r.before[cpu]) == Some(1)
                && r.after[1 - cpu] == r.before[1 - cpu],
            "expected exactly one RTC IRQ on CPU{cpu} alone: {r:?}"
        );
        Ok(r)
    };
    let first = report(1)?;
    let second = report(0)?;
    // Retargeting between alarms must not generate another interrupt.
    ensure!(
        rows.next().is_none() && first.irq == second.irq && first.after == second.before,
        "inconsistent RTC IRQ history"
    );
    Ok([first, second])
}

fn retired_owners(reply: &str) -> Result<[u8; 2]> {
    let mut rows = reply
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("vcpu="));
    let mut owners = [0; 2];
    for (cpu, owner) in owners.iter_mut().enumerate() {
        let (index, rest) = rows
            .next()
            .context("missing vCPU")?
            .split_once(" owner=")
            .context("missing owner")?;
        let (id, state) = rest.split_once(" state=").context("missing vCPU state")?;
        *owner = id.parse()?;
        ensure!(
            index.parse::<usize>()? == cpu
                && *owner < 2
                && state == "Stopped retired=true run=Stopped",
            "vCPU{cpu} not retired on its owner"
        );
    }
    ensure!(
        rows.next().is_none() && owners[0] != owners[1],
        "expected distinct physical owners"
    );
    Ok(owners)
}

fn files(reply: &str, epoch: u32, backend: Backend) -> Result<&str> {
    let mut rows = reply.lines().filter(|l| l.starts_with("irq-file "));
    if backend == Backend::Plic {
        ensure!(rows.next().is_none(), "unexpected IMSIC on PLIC guest");
        return Ok("PLIC");
    }
    let row = rows.next().context("missing IMSIC snapshot")?;
    ensure!(rows.next().is_none(), "ambiguous IMSIC snapshot");
    let mut fields = row.split_ascii_whitespace().skip(1);
    let mut field = |prefix: &str| {
        fields
            .next()
            .and_then(|v| v.strip_prefix(prefix))
            .context("invalid IMSIC snapshot")
    };
    ensure!(
        field("epoch=")?.parse::<u32>()? == epoch,
        "stale IMSIC snapshot"
    );
    let mask = u64::from_str_radix(field("loaded_vcpus=0x")?, 16)?;
    let loads: u64 = field("loads=")?.parse()?;
    let puts: u64 = field("puts=")?.parse()?;
    let merges: u64 = field("merges=")?.parse()?;
    let injected: u64 = field("injected=")?.parse()?;
    ensure!(
        field("bound=0x")? == "0" && fields.next().is_none() && loads == puts,
        "joined guest still owns an IMSIC file"
    );
    match backend {
        Backend::NativeImsic => ensure!(
            mask == 3 && loads >= 2,
            "both vCPUs must have used hardware files"
        ),
        Backend::SoftwareImsic => ensure!(
            (mask, loads, merges, injected) == (0, 0, 0, 0),
            "not software-only IMSIC"
        ),
        Backend::Plic => unreachable!(),
    }
    Ok(row)
}

#[cfg(test)]
mod tests;
