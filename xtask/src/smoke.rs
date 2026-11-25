//! Enforce a serial/clock budget in one QEMU run and validate source-specific evidence.
//! Ownership/Lock Ordering: controller owns the child; scratch outlives child exit.
//! Invariants: no downloads, supplied disks, uplink, or host networking in smoke.
mod archive;
mod controller;
mod firmware;
mod mailbox;
mod mailbox_run;
mod native_ipi;
mod native_timer;
mod provision;
mod smp_irq;
mod volumes;
use crate::{
    args::Options,
    build,
    config::{Board, Disk, Input, Vm},
    launch::{self, Mode, Platform},
    media::{self, Defaults, Scratch},
    process::Session,
};
use anyhow::{Context, Result, bail, ensure};
pub use archive::archive;
use controller::Controller;
use pauli_devices::console::Source;
use std::{
    fs::{self, File},
    io::Read,
    path::Path,
    time::{Duration, Instant},
};

const ALPHA: &str = "alpha";
const BETA: &str = "beta";
const READY: &str = "[probe] ready";
const PONG: &str = "[probe] pong";
const MAX_FIXTURE_BYTES: u64 = 1024 * 1024;
#[derive(Clone, Copy, PartialEq, Eq)]
enum Suite {
    Hv,
    Firmware(firmware::Case),
    Linux,
    Smp,
    SmpIrq,
    Instances,
    Net,
    Storage,
    Provision(provision::Scale),
    Volumes,
}
impl Suite {
    fn parse(value: &str, domains: Option<&str>) -> Result<Self> {
        ensure!(
            value == "provision" || domains.is_none(),
            "--domains requires --suite provision"
        );
        Ok(match value {
            "hv" => Self::Hv,
            "firmware" => Self::Firmware(firmware::Case::Timing([false, true, true, false])),
            "firmware-idle" => Self::Firmware(firmware::Case::Timing([false; 4])),
            "firmware-timer" => Self::Firmware(firmware::Case::VsTimer),
            "firmware-timer-multi" => Self::Firmware(firmware::Case::VsTimerMulti),
            "firmware-timer-cohort" => Self::Firmware(firmware::Case::VsTimerCohort),
            "firmware-timer-smp" => Self::Firmware(firmware::Case::VsTimerSmp),
            "firmware-irq" => Self::Firmware(firmware::Case::ConsoleIrq { remote: false }),
            "firmware-irq-smp" => Self::Firmware(firmware::Case::ConsoleIrq { remote: true }),
            "firmware-irq-latency" => Self::Firmware(firmware::Case::IrqLatency { scale: false }),
            "firmware-irq-scale" => Self::Firmware(firmware::Case::IrqLatency { scale: true }),
            "linux" => Self::Linux,
            "smp" => Self::Smp,
            "smp-irq" => Self::SmpIrq,
            "instances" => Self::Instances,
            "net" => Self::Net,
            "storage" => Self::Storage,
            "volumes" => Self::Volumes,
            "provision" => Self::Provision(provision::Scale::parse(domains)?),
            _ => bail!("unknown smoke suite {value}"),
        })
    }
    fn extra_mib(self) -> u64 {
        match self {
            Self::Provision(scale) => scale.extra_mib(),
            Self::Volumes => 224, // Two 96-MiB children, staging, alignment and private tables.
            _ => 0,
        }
    }
    fn samples_output(self, raw: Option<&str>) -> Result<Option<&Path>> {
        let Some(path) = raw else { return Ok(None) };
        ensure!(
            matches!(self, Self::Firmware(firmware::Case::IrqLatency { .. })),
            "--samples requires --suite firmware-irq-latency or firmware-irq-scale"
        );
        ensure!(!path.is_empty(), "--samples needs a nonempty path");
        // As with image --output, relative paths belong to the caller, not to scratch.
        Ok(Some(Path::new(path)))
    }
    fn checked_options(self, options: &Options) -> Result<Option<&Path>> {
        let host = options.value("host-elf").map(Path::new);
        let allowed: &[&str] = match (self, host) {
            // Comparison hosts use the same fixed topology and release inputs as built hosts.
            (Self::Firmware(case), _) if case.is_vs_timer() => &[
                "board",
                "suite",
                "timer-elf",
                "service-elf",
                "host-elf",
                "timeout-secs",
                "dry-run",
            ],
            (Self::Firmware(firmware::Case::Timing(_)), Some(_)) => &[
                "board",
                "suite",
                "rtos-elf",
                "service-elf",
                "host-elf",
                "timeout-secs",
                "dry-run",
            ],
            (_, Some(_)) => bail!("--host-elf requires a firmware timing suite"),
            (_, None) => &[
                "board",
                "suite",
                "domains",
                "host-cpus",
                "kernel",
                "timeout-secs",
                "features",
                "aia-host",
                "dry-run",
                "rtos-elf",
                "service-elf",
                "samples",
            ],
        };
        options.only(allowed)?;
        Ok(host)
    }
    fn firmware_profile(self, options: &Options) -> Result<Option<(u8, Vec<Vm>)>> {
        let Self::Firmware(case) = self else {
            ensure!(
                !options.flag("rtos-elf") && !options.flag("service-elf"),
                "firmware inputs require a firmware smoke suite"
            );
            return Ok(None);
        };
        let control_key = if case.is_vs_timer() {
            "timer-elf"
        } else {
            "rtos-elf"
        };
        let cpus = case.harts();
        Ok(Some((
            cpus,
            crate::firmware::pair_inputs(options, control_key, cpus)?,
        )))
    }
    fn host_cpus(
        self,
        options: &Options,
        board: &Board,
        default: u8,
        vms: &mut [Vm],
    ) -> Result<u8> {
        if !options.flag("host-cpus") {
            return Ok(default);
        }
        ensure!(
            matches!(self, Self::Smp | Self::Provision(_)),
            "--host-cpus requires --suite smp or provision"
        );
        let count = options.number("host-cpus", default.into(), board.max_cpus.into())? as u8;
        if self == Self::Smp {
            // Exercise both ends, irrespective of which physical hart OpenSBI boots.
            let mut pins = [0; pauli_config::MAX_GUEST_VCPUS as usize];
            pins[1] = count - 1;
            vms[0].config.cpu_affinity = pauli_config::CpuAffinity::Physical(pins);
        }
        Ok(count)
    }
    fn profile(self, scratch: &mut Scratch, aia: bool) -> Result<(u8, Vec<Vm>)> {
        let (a, b) = match self {
            Self::Hv | Self::Firmware(_) => return Ok((1, Vec::new())),
            Self::Linux => (b'L', None),
            Self::Smp => (b'S', None),
            Self::SmpIrq => (b'R', None),
            Self::Instances => (b'I', Some(b'I')),
            Self::Net => (b'A', Some(b'B')),
            Self::Storage | Self::Volumes => (b'D', Some(b'E')),
            Self::Provision(_) => (b'V', None),
        };
        let mut vms = vec![Vm::probe(ALPHA, a)?];
        if let Some(mode) = b {
            vms.push(Vm::probe(BETA, mode)?);
        }
        if matches!(self, Self::Smp | Self::SmpIrq) {
            vms[0].config.vcpus = 2;
        }
        if self == Self::Volumes {
            vms[1].config.autostart = false; // Diskless template for the second tenant.
        }
        for (slot, vm) in vms.iter_mut().enumerate() {
            vm.config.aia = aia;
            if self == Self::Net {
                vm.config.net_peer = Some(1 - slot as u8);
            }
            if self == Self::Storage {
                let (path, file) = scratch.file(&format!("disk-{slot}.raw"))?;
                file.set_len(256 * 1024)?;
                vm.disk = Some(Input::local(&path));
                vm.disk_mode = Disk::Writable;
            }
        }
        let cpus = if b.is_some() || matches!(self, Self::Smp | Self::SmpIrq | Self::Provision(_)) {
            2
        } else {
            1
        };
        Ok((cpus, vms))
    }
}

pub fn run(root: &Path, board: &Board, options: &Options) -> Result<()> {
    if let Some(suite @ ("mailbox" | "ipi-handoff")) = options.value("suite") {
        return mailbox_run::run(root, board, options, suite);
    }
    if let Some(name @ ("native-timer" | "native-ipi")) = options.value("suite") {
        // No VM manifest, loader, guest console or HS monitor in this control.
        options.only(&["board", "suite", "timeout-secs", "dry-run"])?;
        let seconds = options.number("timeout-secs", 40, 180)?;
        let platform = Platform::new(board, options, &[], Mode::Normal)?;
        let (example, cpus) = if name == "native-ipi" {
            ("native_ipi", 2)
        } else {
            ("native_timer", 1)
        };
        let elf = build::board_example(root, board, example)?;
        let command = platform.machine(&elf, cpus, media::FIRMWARE_RAM_MIB);
        println!("smoke {name}: {command:?}");
        if !options.flag("dry-run") {
            verify_process(command, seconds, |c| match name {
                "native-ipi" => native_ipi::verify(c),
                _ => native_timer::verify(c),
            })?;
            println!("smoke {name}: PASS (clean QEMU exit, wall deadline {seconds}s)");
        }
        return Ok(());
    }
    let name = options.value("suite").context("--suite is required")?;
    let suite = Suite::parse(name, options.value("domains"))?;
    let supplied = suite.checked_options(options)?;
    let pair = suite.firmware_profile(options)?;
    if let Some(path) = supplied {
        ensure!(board.qemu, "firmware timing requires a QEMU board");
        crate::elf::inspect(path, board).context("invalid firmware comparison host ELF")?;
    }
    let output = suite.samples_output(options.value("samples"))?;
    if let Some(path) = output {
        // Refuse a repeat invocation before spending its QEMU budget; create_new
        // after clean exit still protects against a concurrent writer.
        ensure!(
            !path.try_exists()?,
            "IRQ sample output already exists: {}",
            path.display()
        );
    }
    let firmware = matches!(suite, Suite::Firmware(_));
    let kernel = launch::input(options, "kernel");
    ensure!(
        kernel.is_some() == !(suite == Suite::Hv || firmware),
        "--kernel IMAGE is required for Linux suites only"
    );
    let seconds = options.number("timeout-secs", 40, 180)?;
    let mut scratch = Scratch::new(root)?;
    let (cpus, mut vms) = match pair {
        Some(pair) => pair,
        None => suite.profile(&mut scratch, options.flag("aia-host"))?,
    };
    let cpus = suite.host_cpus(options, board, cpus, &mut vms)?;
    let managed = if suite == Suite::Volumes {
        volumes::media(&mut scratch)?
    } else {
        Vec::new()
    };
    let mode = match suite {
        Suite::Hv => Mode::HvSelftest,
        Suite::SmpIrq
        | Suite::Firmware(
            firmware::Case::ConsoleIrq { remote: true } | firmware::Case::IrqLatency { .. },
        ) => Mode::Observed,
        _ => Mode::Normal,
    };
    let platform = Platform::new(board, options, &vms, mode)?;
    let media = if suite == Suite::Hv {
        None
    } else {
        let fixture = if firmware {
            None
        } else {
            Some(fixture(root, board)?)
        };
        let mut media = media::prepare(
            &mut scratch,
            board,
            &mut vms,
            Defaults {
                host_mib: None,
                kernel: kernel.as_ref(),
                initrd: None,
                disk: None,
                managed: &managed,
                probe: fixture.as_deref(),
            },
            cpus,
        )?;
        media.ram_mib = media
            .ram_mib
            .checked_add(suite.extra_mib())
            .context("host RAM overflow")?;
        Some(media)
    };
    let elf = match supplied {
        Some(path) => path.to_owned(),
        None => platform.build(root)?,
    };
    // Only this invocation's temporary disks may persist writes for offline readback.
    let command = platform.command(
        &elf,
        cpus,
        media.as_ref(),
        matches!(suite, Suite::Storage | Suite::Volumes),
    )?;
    println!("smoke {name}: {command:?}");
    if options.flag("dry-run") {
        return Ok(());
    }
    let mut samples = firmware::Capture::default();
    verify_process(command, seconds, |c| {
        verify(c, suite, cpus, options, &mut samples)
    })?;
    if suite == Suite::Storage {
        verify_disks(&vms)?;
    }
    for (volume, pattern) in managed.iter().zip(b"DE") {
        verify_sector(&volume.input.path, *pattern)?;
    }
    // A timeout, failed check, or failed process exit must never publish a scored CSV.
    if let Some(path) = output {
        samples.save(path)?;
    }
    println!("smoke {name}: PASS (clean QEMU exit, wall deadline {seconds}s)");
    Ok(())
}
fn verify_process(
    mut command: std::process::Command,
    seconds: u64,
    verify: impl FnOnce(&mut Controller) -> Result<()>,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut c = Controller::with_deadline(Session::start_until(&mut command, deadline)?, deadline);
    let result = verify(&mut c).and_then(|()| c.finish());
    let expired = Instant::now() >= deadline;
    if let Err(error) = result {
        c.report_failure();
        return Err(if expired {
            error.context("QEMU exceeded the hard deadline")
        } else {
            error
        });
    }
    ensure!(!expired, "QEMU exceeded the hard deadline");
    Ok(())
}
fn fixture(root: &Path, board: &Board) -> Result<Vec<u8>> {
    let path = build::binary(root, &board.target, "pauli-linux-probe", None, false)?;
    let mut fixture = Vec::new();
    File::open(path)?
        .take(MAX_FIXTURE_BYTES + 1)
        .read_to_end(&mut fixture)?;
    ensure!(
        fixture.starts_with(b"\x7fELF\x02\x01") && fixture.len() as u64 <= MAX_FIXTURE_BYTES,
        "invalid Linux probe ELF"
    );
    Ok(fixture)
}
fn verify(
    c: &mut Controller,
    suite: Suite,
    cpus: u8,
    options: &Options,
    samples: &mut firmware::Capture,
) -> Result<()> {
    if suite == Suite::Hv {
        c.wait(
            Source::Host,
            0,
            "HV selftest PASS: entry/resume, native timer preemption, G-stage RX isolation",
        )?;
        return Ok(());
    }
    c.wait(Source::Host, 0, "hv> ")?;
    let feature = |name| {
        options
            .value("features")
            .is_some_and(|list| list.split(',').any(|f| f == name))
    };
    if suite == Suite::SmpIrq {
        let backend = smp_irq::Backend::new(options.flag("aia-host"), feature("aia-guest-file"));
        smp_irq::verify(c, backend)?;
        return c.shutdown();
    }
    if let Suite::Firmware(case) = suite {
        firmware::verify(c, case, feature("selftest"), samples)?;
        return c.shutdown();
    }
    let a = c.status(ALPHA, "Ready")?;
    if suite == Suite::Smp && options.flag("host-cpus") {
        let last = cpus - 1;
        c.wait(Source::Host, c.last_command(), "vcpu=0 owner=0 ")?;
        c.wait(
            Source::Host,
            c.last_command(),
            &format!("vcpu=1 owner={last} "),
        )?;
    }
    let guest = Source::Guest(a);
    c.wait(guest, 0, "[probe] PID1 TIME FD PASS")?;
    c.wait(guest, 0, READY)?;
    match suite {
        Suite::Linux | Suite::Smp => {
            if suite == Suite::Smp {
                c.wait(guest, 0, "[probe] SMP HSM affinity PASS")?;
            }
            c.attach(ALPHA, a)?;
            c.guest(a, b'q', "[probe] Poweroff")?;
            c.terminal(1, a, "Stopped")?;
            c.command("vm status alpha")?;
            c.wait(Source::Host, c.last_command(), "VM 1 exit=Shutdown")?;
        }
        Suite::Net | Suite::Instances | Suite::Storage => verify_pair(c, suite, a)?,
        Suite::Provision(scale) => scale.verify(c, a)?,
        Suite::Volumes => volumes::verify(c, a)?,
        Suite::Hv | Suite::SmpIrq | Suite::Firmware(_) => {
            unreachable!("handled before ordinary Linux VM status")
        }
    }
    c.shutdown()
}
fn verify_pair(c: &mut Controller, suite: Suite, a: u32) -> Result<()> {
    let b = c.status(BETA, "Ready")?;
    c.wait(Source::Guest(b), 0, READY)?;
    respond(c, ALPHA, a, b'p', PONG)?;
    respond(c, BETA, b, b'p', PONG)?;
    match suite {
        Suite::Net => {
            for epoch in [a, b] {
                c.wait(Source::Guest(epoch), 0, "[probe] NET IPv4 UDP PASS")?;
            }
        }
        Suite::Instances | Suite::Storage => {
            if suite == Suite::Storage {
                respond(c, ALPHA, a, b'w', "[probe] disk WRITE FLUSH PASS")?;
                respond(c, BETA, b, b'z', "[probe] disk ZERO PASS")?;
                respond(c, BETA, b, b'w', "[probe] disk WRITE FLUSH PASS")?;
                respond(c, ALPHA, a, b'r', "[probe] disk READ PASS")?;
            }
            c.attach(ALPHA, a)?;
            c.guest(a, b'f', "[probe] touching unmapped guest page")?;
            c.terminal(1, a, "Faulted")?;
            c.command("vm status alpha")?;
            c.wait(Source::Host, c.last_command(), "VM 1 exit=Fault cause=0x15")?;
            respond(c, BETA, b, b'p', PONG)?;
            c.command("vm start alpha")?;
            let fresh = c.status(ALPHA, "Ready")?;
            ensure!(fresh != a, "cold restart reused the old epoch");
            c.wait(Source::Guest(fresh), 0, READY)?;
            if suite == Suite::Storage {
                respond(c, ALPHA, fresh, b'r', "[probe] disk READ PASS")?;
            }
            respond(c, ALPHA, fresh, b'p', PONG)?;
            c.command("vm stop alpha")?;
            c.terminal(1, fresh, "Stopped")?;
        }
        _ => unreachable!("single VM suites handled before verify_pair"),
    }
    respond(c, BETA, b, b'p', PONG)
}
fn respond(c: &mut Controller, vm: &str, epoch: u32, key: u8, marker: &str) -> Result<()> {
    c.attach(vm, epoch)?;
    c.guest(epoch, key, marker)?;
    c.detach()
}
fn verify_disks(vms: &[Vm]) -> Result<()> {
    for vm in vms {
        let disk = &vm.disk.as_ref().context("missing smoke disk")?.path;
        let pattern = vm.probe.context("missing disk pattern")?;
        verify_sector(disk, pattern)?;
    }
    Ok(())
}
fn verify_sector(disk: &Path, pattern: u8) -> Result<()> {
    ensure!(
        fs::metadata(disk)?.len() == 256 * 1024,
        "smoke disk size changed"
    );
    let mut bytes = [0u8; 512];
    File::open(disk)?.read_exact(&mut bytes)?;
    ensure!(
        bytes.iter().all(|&byte| byte == pattern),
        "host raw disk persistence mismatch"
    );
    Ok(())
}
