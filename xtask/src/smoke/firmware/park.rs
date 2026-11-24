//! Positive pre-window evidence: the control guest's owner published a WFI exit.
use super::{Controller, REPORT, Report, Serial, Source, line};
use anyhow::{Context, Result, bail, ensure};

pub(super) fn control_waiting<T: Serial>(c: &mut Controller<T>, epoch: u32) -> Result<()> {
    let header = format!("VM id=1 epoch={epoch} name=control state=Ready vcpus=1");
    let mut wait_for_report = true;
    loop {
        c.command("vm status control")?;
        let fresh = std::str::from_utf8(&c.log(Source::Host)[c.last_command()..])?;
        let (reply, _) = fresh
            .split_once("\nhv> ")
            .context("incomplete control status")?;
        let mut rows = reply
            .lines()
            .map(str::trim_start)
            .filter(|row| row.starts_with("VM ") || row.starts_with("vcpu"));
        let vm = rows.next().context("missing control VM status")?;
        ensure!(
            vm.rsplit_once(' ')
                .is_some_and(|(fields, memory)| fields == header && memory.starts_with("memory=")),
            "unexpected control VM status: {vm}"
        );
        let cpu = rows.next().context("missing control vCPU status")?;
        ensure!(rows.next().is_none(), "duplicate control status rows");
        match cpu {
            "vcpu=0 owner=0 state=Started retired=false run=Waiting" => return Ok(()),
            "vcpu=0 owner=0 state=Started retired=false run=Runnable"
            | "vcpu=0 owner=0 state=StartPending retired=false run=Starting" => {}
            _ => bail!("unexpected control vCPU status: {cpu}"),
        }
        // Space retries by a guest report, but also sample between reports:
        // querying only just after printing could always catch the same phase.
        if wait_for_report {
            let src = Source::Guest(epoch);
            let mut cursor = c.log(src).len();
            Report::parse(line(c, src, &mut cursor, REPORT)?)?;
        }
        wait_for_report = !wait_for_report;
    }
}

#[cfg(test)]
mod tests;
