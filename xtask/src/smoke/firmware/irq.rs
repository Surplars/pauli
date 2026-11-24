//! Verify IRQ-only UART wake, service restart, clone isolation and RTOS continuity.
//! Ownership/Lock Ordering: one host controller owns the session; no shared locks.
//! Invariants: the wake gate parks before each byte; only the addressed epoch supplies its ACK.
use super::{Controller, REPORT, Serial, Source, fresh_window};
use anyhow::{Context, Result, bail, ensure};
pub(super) mod latency;

const READY: &str = "[irq] ready\n";
const OLD: &[(u8, &str)] = &[
    (b'A', "[irq] rx=A seq=0000000000000001\n"),
    (b'B', "[irq] rx=B seq=0000000000000002\n"),
    (b'!', "[irq] hold-next\n"),
    (b'Z', "[irq] held-unread\n"),
];
const NEW: &[(u8, &str)] = &[
    (b'C', "[irq] rx=C seq=0000000000000001\n"),
    (b'D', "[irq] rx=D seq=0000000000000002\n"),
];
const COPY: (u8, &str) = (b'K', "[irq] rx=K seq=0000000000000001\n");
const THIRD: (u8, &str) = (b'E', "[irq] rx=E seq=0000000000000003\n");

pub(super) fn verify(c: &mut Controller, remote: bool) -> Result<()> {
    let placement = Placement::parse(c.log(Source::Host), remote)?;
    let control = c.status("control", "Ready")?;
    cpu(&c.log(Source::Host)[c.last_command()..], placement.boot)?;
    let old = c.status("service", "Ready")?;
    let before = fresh_window(c, control)?;
    exchange(c, control, old, placement, OLD)?;

    // Z is still in the old UART when stop joins the service. An uncleared
    // byte/line/claim must not satisfy the new boot's C/D protocol.
    c.command("vm stop service")?;
    c.terminal(2, old, "Stopped")?;
    let stopped = fresh_window(c, control)?;
    c.command("vm start service")?;
    let fresh = c.status("service", "Ready")?;
    ensure!(fresh != old, "service cold boot reused epoch {old}");
    exchange(c, control, fresh, placement, NEW)?;
    let after = fresh_window(c, control)?;
    ensure!(
        before < stopped && stopped < after && c.status("control", "Ready")? == control,
        "RTOS release count or epoch did not survive peer stop/restart"
    );
    check_log(c.log(Source::Guest(old)), OLD)?;
    check_log(c.log(Source::Guest(fresh)), NEW)?;
    if !remote {
        verify_clone(c, control, fresh)?;
    }
    println!(
        "firmware IRQ: monitor/control owner={}, service owner={}; parked RX/claim, held byte across stop, clean restart {old}->{fresh}; RTOS epoch={control} samples={before}/{stopped}/{after}: PASS",
        placement.boot, placement.service
    );
    Ok(())
}

// Clone a live template after it has already consumed two bytes. Test new
// state and continued source state on distinct epoch consoles, without sleeps.
fn verify_clone<T: Serial>(c: &mut Controller<T>, control: u32, service: u32) -> Result<()> {
    c.command("vm create service copy")?;
    c.wait(
        Source::Host,
        c.last_command(),
        "VM 3 created: copy (stopped)\n",
    )?;
    c.command("vm start copy")?;
    let copy = c.status("copy", "Ready")?;
    ensure!(
        copy != service && copy != control,
        "clone reused a live epoch"
    );
    c.wait(Source::Guest(copy), 0, READY)?;
    for (name, epoch, (key, ack)) in [("copy", copy, COPY), ("service", service, THIRD)] {
        c.attach(name, epoch)?;
        c.guest(epoch, key, ack)?;
        c.detach()?;
    }
    c.command("vm stop copy")?;
    c.terminal(3, copy, "Stopped")?;
    c.destroy(3, "copy")?; // Requires a fresh positive reply after retirement.
    ensure!(
        c.status("control", "Ready")? == control,
        "clone teardown changed RTOS epoch"
    );
    check_log(c.log(Source::Guest(copy)), &[COPY])?;
    check_log(c.log(Source::Guest(service)), &[NEW[0], NEW[1], THIRD])?;
    println!(
        "firmware clone: epoch={copy} seq=1, source epoch={service} seq=3; stopped and destroyed: PASS"
    );
    Ok(())
}

fn exchange<T: Serial>(
    c: &mut Controller<T>,
    control: u32,
    epoch: u32,
    placement: Placement,
    steps: &[(u8, &str)],
) -> Result<()> {
    let src = Source::Guest(epoch);
    c.wait(src, 0, READY)?;
    let mut software = 0;
    for (done, &(byte, ack)) in steps.iter().enumerate() {
        let before = parked(c, Some(control), epoch, placement)?;
        check_log(c.log(src), &steps[..done])?;
        c.attach("service", epoch)?;
        c.guest(epoch, byte, ack)?; // Starts at the epoch's cursor immediately before send.
        c.detach()?;
        if let Some(before) = before {
            // A claim/deassert can cause a second kick, so the count need not be exactly one.
            let (_, after) = worker(c, placement.service)?;
            software += wake_delta(before, after)?;
        }
    }
    parked(c, Some(control), epoch, placement)?;
    if placement.remote() {
        println!(
            "firmware IRQ: epoch={epoch} remote owner={} acknowledged={} disarmed-WFI software returns={software}",
            placement.service,
            steps.len()
        );
    }
    check_log(c.log(src), steps)
}

fn check_log(log: &[u8], steps: &[(u8, &str)]) -> Result<()> {
    let expected = READY
        .bytes()
        .chain(steps.iter().flat_map(|(_, ack)| ack.bytes()));
    ensure!(
        expected.eq(log.iter().copied()),
        "unexpected IRQ transcript: {}",
        String::from_utf8_lossy(log)
    );
    Ok(())
}

fn parked<T: Serial>(
    c: &mut Controller<T>,
    control: Option<u32>,
    epoch: u32,
    placement: Placement,
) -> Result<Option<u64>> {
    loop {
        ensure!(
            c.status("service", "Ready")? == epoch,
            "unexpected service epoch"
        );
        if waiting(&c.log(Source::Host)[c.last_command()..], placement.service)? {
            if !placement.remote() {
                return Ok(None);
            }
            let (parked, software) = worker(c, placement.service)?;
            if parked {
                return Ok(Some(software));
            }
        }
        // The banner/ACK can reach the host one exit before WFI. Give the
        // guest a release window before querying again (same hard deadline).
        if let Some(control) = control {
            let peer = Source::Guest(control);
            c.wait(peer, c.log(peer).len(), REPORT)?;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Placement {
    boot: u8,
    service: u8,
}
impl Placement {
    fn remote(self) -> bool {
        self.service != self.boot
    }

    // The board reports logical owner indices. OpenSBI may start either hart.
    fn parse(log: &[u8], remote: bool) -> Result<Self> {
        let expected = if remote { 2 } else { 1 };
        let (boot, _) = crate::smoke::controller::native_board(log, expected)?;
        Ok(Self {
            boot,
            service: if remote { 1 - boot } else { boot },
        })
    }
}

// Only the named worker's disarmed native WFI bracket and SSIP-only returns count.
fn worker<T: Serial>(c: &mut Controller<T>, owner: u8) -> Result<(bool, u64)> {
    c.command("vm capacity")?;
    worker_snapshot(&c.log(Source::Host)[c.last_command()..], owner)
}

fn worker_snapshot(reply: &[u8], owner: u8) -> Result<(bool, u64)> {
    let mut rows = std::str::from_utf8(reply)?
        .lines()
        .filter_map(|row| row.strip_prefix("worker-idle owner="));
    let row = rows
        .next()
        .context("no worker idle sample; expected selftest")?;
    ensure!(rows.next().is_none(), "expected one remote worker");
    let (id, rest) = row.split_once(" parked=").context("invalid worker owner")?;
    ensure!(id.parse::<u8>()? == owner, "wrong idle worker: {row}");
    let (parked, software) = rest
        .split_once(" software=")
        .context("invalid worker counters")?;
    Ok((parked.parse()?, software.parse()?))
}

fn wake_delta(before: u64, after: u64) -> Result<u64> {
    ensure!(
        after > before,
        "no new native software wake: {before}->{after}"
    );
    Ok(after - before)
}

pub(super) fn cpu(reply: &[u8], owner: u8) -> Result<&str> {
    let mut rows = std::str::from_utf8(reply)?
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("vcpu="));
    let row = rows.next().context("no vCPU status")?;
    ensure!(rows.next().is_none(), "expected one vCPU");
    let (reported, state) = row
        .strip_prefix("0 owner=")
        .and_then(|row| row.split_once(" state="))
        .context("invalid vCPU status")?;
    ensure!(
        reported.parse::<u8>()? == owner,
        "expected native owner {owner}: {row}"
    );
    Ok(state)
}

fn waiting(reply: &[u8], owner: u8) -> Result<bool> {
    let state = cpu(reply, owner)?;
    match state.strip_prefix("Started retired=false run=") {
        Some("Waiting") => Ok(true),
        Some("Runnable") => Ok(false),
        _ => bail!("unexpected service vCPU status: {state}"),
    }
}

#[cfg(test)]
mod replay;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_scheduler_waiting_not_hsm_started_or_a_stale_row() {
        let row = "  vcpu=0 owner=0 state=Started retired=false run=";
        assert!(waiting(format!("{row}Waiting\nhv> ").as_bytes(), 0).unwrap());
        assert!(!waiting(format!("{row}Runnable\nhv> ").as_bytes(), 0).unwrap());
        assert!(waiting(b"  vcpu=0 owner=0 state=Started retired=false\n", 0).is_err());
        assert!(waiting(format!("{row}Waiting\n{row}Runnable\n").as_bytes(), 0).is_err());
        assert!(waiting(format!("{row}Waiting\n").as_bytes(), 1).is_err());
    }

    #[test]
    fn service_must_run_on_the_worker_regardless_of_boot_order() {
        for boot in [0, 1] {
            let banner =
                format!("OpenSBI\r\n[HV] QEMU virt: 2 harts, boot {boot}, 10000000 Hz\nhv> ");
            assert_eq!(
                Placement::parse(banner.as_bytes(), true).unwrap(),
                Placement {
                    boot,
                    service: 1 - boot
                }
            );
            assert!(Placement::parse(banner.as_bytes(), false).is_err());
        }
        let invalid = b"[HV] QEMU virt: 2 harts, boot 2, 10000000 Hz\n";
        assert!(Placement::parse(invalid, true).is_err());
    }

    #[test]
    fn remote_input_needs_a_fresh_software_wake_on_the_parked_worker() {
        let sample = b"worker-idle owner=0 parked=true software=12\nhv> ";
        assert_eq!(worker_snapshot(sample, 0).unwrap(), (true, 12));
        assert!(worker_snapshot(sample, 1).is_err());
        assert!(worker_snapshot(b"idle-wake software=12\n", 0).is_err());
        let active = b"worker-idle owner=0 parked=false software=13\n";
        assert_eq!(worker_snapshot(active, 0).unwrap(), (false, 13));
        assert!(worker_snapshot(&[sample.as_slice(), b"\n", active].concat(), 0).is_err());
        assert_eq!(wake_delta(12, 14).unwrap(), 2);
        assert!(wake_delta(12, 12).is_err());
        assert!(wake_delta(12, 0).is_err());
    }

    #[test]
    fn restart_requires_clean_exact_sequence() {
        let clean = format!("{READY}{}{}", NEW[0].1, NEW[1].1);
        assert!(check_log(clean.as_bytes(), NEW).is_ok());
        assert!(check_log(clean.replace("rx=C", "rx=Z").as_bytes(), NEW).is_err());
        assert!(check_log(clean.replace("0001", "0003").as_bytes(), NEW).is_err());
        assert!(check_log(format!("{clean}{}", NEW[1].1).as_bytes(), NEW).is_err());
    }
}
