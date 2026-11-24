//! Replay source-framed guest output; neither the fixture nor a timer runs on QEMU.
//! Ownership: one serial reader; successive windows share only the committed byte cursor.
//! Invariant: no stale, partial, wrong-epoch or missing report becomes a sample.
use super::*;
use crate::process::Output;
use crate::smoke::Suite;
use pauli_devices::console::Encoder;
use std::{collections::VecDeque, time::Duration};

#[test]
fn only_uart_latency_suites_can_export_samples() -> Result<()> {
    use std::path::Path;
    let path = "reports/paired.csv";
    for name in ["firmware-irq-latency", "firmware-irq-scale"] {
        let suite = Suite::parse(name, None)?;
        assert_eq!(suite.samples_output(Some(path))?, Some(Path::new(path)));
        assert!(suite.samples_output(Some("")).is_err());
        assert_eq!(suite.samples_output(None)?, None);
    }
    for name in [
        "firmware",
        "firmware-timer",
        "firmware-timer-multi",
        "firmware-timer-cohort",
        "firmware-timer-smp",
        "firmware-irq-smp",
        "hv",
        "smp-irq",
        "provision",
    ] {
        let suite = Suite::parse(name, None)?;
        assert!(suite.samples_output(Some(path)).is_err());
        assert_eq!(suite.samples_output(None)?, None);
    }
    Ok(())
}

#[test]
fn idle_control_changes_only_the_two_middle_scored_loads() -> Result<()> {
    let Suite::Firmware(abba) = Suite::parse("firmware", None)? else {
        panic!("expected diskless firmware");
    };
    let Suite::Firmware(quiet) = Suite::parse("firmware-idle", None)? else {
        panic!("expected the same diskless firmware profile");
    };
    assert_eq!(abba, Case::Timing([false, true, true, false]));
    assert_eq!(quiet, Case::Timing([false; 4]));
    assert!(Suite::parse("firmware-idle", Some("2")).is_err());
    Ok(())
}

#[test]
fn only_default_firmware_timing_suites_accept_a_comparison_host() -> Result<()> {
    use crate::args::Options;
    use std::path::Path;
    let parse = |args: &[&str]| Options::parse_from(args.iter().map(|s| s.to_string()));
    for name in ["firmware", "firmware-idle"] {
        let suite = Suite::parse(name, None)?;
        let options = parse(&["smoke", "--suite", name, "--host-elf", "base.elf"])?;
        assert_eq!(
            suite.checked_options(&options)?,
            Some(Path::new("base.elf"))
        );
        for extra in [
            &["--features", "selftest"][..],
            &["--aia-host"],
            &["--host-cpus", "2"],
        ] {
            let args = [
                &["smoke", "--suite", name, "--host-elf", "base.elf"][..],
                extra,
            ]
            .concat();
            assert!(suite.checked_options(&parse(&args)?).is_err());
        }
    }
    let options = parse(&["smoke", "--host-elf", "base.elf"])?;
    for name in ["firmware-irq", "firmware-irq-latency", "linux", "hv"] {
        assert!(Suite::parse(name, None)?.checked_options(&options).is_err());
    }
    Ok(())
}

#[test]
fn vs_timer_pair_has_fixed_sources_on_one_diskless_hart() -> Result<()> {
    use crate::args::Options;
    use std::path::Path;
    let parse = |args: &[&str]| Options::parse_from(args.iter().map(|s| s.to_string()));
    let suite = Suite::parse("firmware-timer", None)?;
    assert!(suite == Suite::Firmware(Case::VsTimer));
    let base = &[
        "smoke",
        "--suite",
        "firmware-timer",
        "--timer-elf",
        "probe.elf",
        "--service-elf",
        "peer.elf",
        "--dry-run",
        "--timeout-secs",
        "45",
        "--board",
        "qemu-virt-riscv",
    ][..];
    for host in [&[][..], &["--host-elf", "reference.elf"]] {
        let options = parse(&[base, host].concat())?;
        assert_eq!(suite.checked_options(&options)?, host.get(1).map(Path::new));
        let (cpus, vms) = suite.firmware_profile(&options)?.unwrap();
        assert_eq!((cpus, vms.len()), (1, 2));
        for (vm, (name, path, kib)) in vms
            .iter()
            .zip([("control", "probe.elf", 256), ("service", "peer.elf", 32)])
        {
            assert_eq!(vm.firmware.as_ref().unwrap().path, Path::new(path));
            assert_eq!(
                (vm.config.name(), vm.config.vcpus, vm.config.memory_size),
                (name, 1, kib * 1024)
            );
            assert!(
                vm.config.autostart && vm.config.cpu_affinity == pauli_config::CpuAffinity::Automatic
            );
            assert!(vm.disk.is_none() && vm.initrd.is_none() && vm.kernel.is_none());
            assert!(!vm.config.aia && vm.config.net_peer.is_none() && !vm.config.net_uplink);
        }
        for extra in [
            &["--rtos-elf", "old.elf"][..],
            &["--kernel", "Image"],
            &["--disk", "disk.raw"],
            &["--initrd", "initrd"],
            &["--vm", "net.toml"],
            &["--host-cpus", "1"],
            &["--aia-host"],
            &["--features", "selftest"],
            &["--samples", "out.csv"],
        ] {
            assert!(
                suite
                    .checked_options(&parse(&[base, host, extra].concat())?)
                    .is_err(),
                "{extra:?}"
            );
        }
    }
    for partial in [
        &["--service-elf", "peer.elf"][..],
        &["--timer-elf", "probe.elf"],
    ] {
        let options = parse(&[&["smoke", "--suite", "firmware-timer"][..], partial].concat())?;
        assert!(suite.firmware_profile(&options).is_err());
    }
    Ok(())
}

#[test]
fn overcommit_suites_add_clones_at_runtime_not_in_the_boot_image() -> Result<()> {
    use crate::args::Options;
    use pauli_config::CpuAffinity::{Automatic, Workers};
    for (name, cpus, service_owner) in [
        ("firmware-timer-multi", 1, Automatic),
        ("firmware-timer-cohort", 1, Automatic),
        ("firmware-timer-smp", 2, Workers),
    ] {
        let suite = Suite::parse(name, None)?;
        let base = [
            "smoke",
            "--suite",
            name,
            "--timer-elf",
            "probe",
            "--service-elf",
            "worker",
        ];
        let options =
            |extra: &[&str]| Options::parse_from(base.iter().chain(extra).map(|s| (*s).to_owned()));
        for extra in [
            &["--host-cpus", "2"][..],
            &["--features", "selftest"],
            &["--kernel", "Image"],
            &["--disk", "disk.raw"],
            &["--rtos-elf", "not-a-timer"],
            &["--aia-host"],
        ] {
            assert!(suite.checked_options(&options(extra)?).is_err());
        }
        for host in [&[][..], &["--host-elf", "reference"]] {
            let options = options(host)?;
            suite.checked_options(&options)?;
            let (harts, vms) = suite.firmware_profile(&options)?.unwrap();
            assert_eq!((harts, vms.len()), (cpus, 2));
            for (vm, (name, owner)) in vms
                .iter()
                .zip([("control", Automatic), ("service", service_owner)])
            {
                assert_eq!((vm.config.name(), vm.config.cpu_affinity), (name, owner));
                assert!(vm.config.autostart && vm.config.vcpus == 1);
                assert!(vm.disk.is_none() && vm.kernel.is_none() && vm.initrd.is_none());
                assert!(!vm.config.net_uplink && vm.config.net_peer.is_none());
            }
        }
    }
    Ok(())
}

#[test]
fn timer_input_is_rejected_by_other_suites_before_staging() -> Result<()> {
    use crate::{args::Options, config::Board, smoke};
    use std::path::Path;
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let board = Board::load(&workspace, "qemu-virt-riscv")?;
    // A regular file cannot contain scratch directories or build outputs.
    let no_staging = workspace.join("Cargo.toml");
    for name in [
        "hv",
        "firmware",
        "firmware-idle",
        "firmware-irq",
        "firmware-irq-smp",
        "firmware-irq-latency",
        "firmware-irq-scale",
        "native-timer",
        "native-ipi",
        "mailbox",
        "ipi-handoff",
        "linux",
        "smp",
        "smp-irq",
        "instances",
        "net",
        "storage",
        "provision",
        "volumes",
    ] {
        let options = Options::parse_from(
            [
                "smoke",
                "--suite",
                name,
                "--timer-elf",
                "probe.elf",
                "--dry-run",
            ]
            .into_iter()
            .map(str::to_owned),
        )?;
        let error = smoke::run(&no_staging, &board, &options).unwrap_err();
        assert!(error.to_string().contains("--timer-elf"), "{name}: {error}");
    }
    Ok(())
}

#[test]
fn vs_timer_comparison_checks_the_host_elf_before_staging_guests() -> Result<()> {
    use crate::{args::Options, config::Board, smoke};
    use std::path::Path;
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let board = Board::load(&workspace, "qemu-virt-riscv")?;
    let not_an_elf = workspace.join("Cargo.toml");
    let options = Options::parse_from(
        [
            "smoke",
            "--suite",
            "firmware-timer",
            "--timer-elf",
            "absent-probe",
            "--service-elf",
            "absent-peer",
            "--host-elf",
            not_an_elf.to_str().unwrap(),
            "--dry-run",
        ]
        .into_iter()
        .map(str::to_owned),
    )?;
    let error = smoke::run(&not_an_elf, &board, &options).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("invalid firmware comparison host ELF")
    );
    Ok(())
}

#[test]
fn irq_suites_select_their_native_topology() -> Result<()> {
    for (name, remote, harts) in [("firmware-irq", false, 1), ("firmware-irq-smp", true, 2)] {
        let Suite::Firmware(case) = Suite::parse(name, None)? else {
            panic!("expected diskless firmware");
        };
        assert_eq!(case, Case::ConsoleIrq { remote });
        assert_eq!(case.harts(), harts);
    }
    for (name, scale) in [
        ("firmware-irq-latency", false),
        ("firmware-irq-scale", true),
    ] {
        let Suite::Firmware(latency) = Suite::parse(name, None)? else {
            panic!("expected diskless IRQ timing suite");
        };
        assert_eq!(latency, Case::IrqLatency { scale });
        assert_eq!(latency.harts(), 2);
        assert!(Suite::parse(name, Some("8")).is_err());
    }
    Ok(())
}

pub(super) struct Replay(pub(super) VecDeque<Vec<u8>>);
impl Serial for Replay {
    fn send(&mut self, _: &[u8]) -> Result<()> {
        anyhow::bail!("read-only replay")
    }
    fn receive(&mut self, _: Duration) -> Result<Output> {
        Ok(self.0.pop_front().map_or(Output::End, Output::Data))
    }
}
#[test]
fn a_busy_ack_or_stale_work_cannot_prove_progress_before_stop() -> Result<()> {
    // Stage one UART reply per command. Keep a stale positive work report in
    // the log so a parser reusing an old reply would incorrectly pass.
    struct Wire(Replay, VecDeque<u8>);
    impl Serial for Wire {
        fn send(&mut self, bytes: &[u8]) -> Result<()> {
            ensure!(
                bytes.len() == 1 && self.1.pop_front() == Some(bytes[0]),
                "unexpected command"
            );
            Ok(())
        }
        fn receive(&mut self, timeout: Duration) -> Result<Output> {
            self.0.receive(timeout)
        }
    }
    for (last, current, valid) in [
        (0, 0, false),
        (32, 32, false),
        (32, 16, false),
        (32, 64, true),
    ] {
        let mut encoder = Encoder::new();
        let mut data = Vec::new();
        encoder.attach(2, |b| data.push(b));
        let mut chunks = VecDeque::new();
        for line in [
            "[bare] work=ffffffffffffffff\nready",
            "[bare] busy\n",
            &format!("[bare] work={current:016x}\n"),
        ] {
            for b in line.bytes() {
                encoder.write(Source::Guest(2), b, |b| data.push(b));
            }
            chunks.push_back(std::mem::take(&mut data));
        }
        let mut c = Controller::new(
            Wire(Replay(chunks), VecDeque::from(*b"b?")),
            Duration::from_secs(2),
        );
        c.wait(Source::Guest(2), 0, "ready")?;
        assert_eq!(confirm_busy(&mut c, 2, last).ok(), valid.then_some(current));
    }
    Ok(())
}
fn replay(chunk: usize, skip: bool) -> Controller<Replay> {
    let mut data = Vec::new();
    let mut encoder = Encoder::new();
    let mut emit = |epoch, text: &str| {
        for b in text.bytes() {
            encoder.write(Source::Guest(epoch), b, |b| data.push(b));
        }
    };
    emit(
        1,
        "[rtos] samples=32 max_late_ticks=100 missed=5 window_max_ticks=100\n",
    );
    emit(2, "[rtos] anchor=992 missed=100\n");
    emit(1, "[rtos] anchor=32 missed=5\r\n");
    if !skip {
        emit(1, "[rtos] samples=64 max_late_ticks=100 missed=5 window_");
        emit(
            2,
            "[rtos] samples=64 max_late_ticks=999 missed=4 window_max_ticks=999\n",
        );
        emit(1, "max_ticks=3\n");
    }
    emit(
        1,
        "[rtos] samples=96 max_late_ticks=100 missed=6 window_max_ticks=10\n",
    );
    let frames = data.chunks(chunk).map(<[u8]>::to_vec).collect();
    Controller::new(Replay(frames), Duration::from_secs(2))
}

#[test]
fn receive_cursor_survives_split_and_coalesced_frames_and_ignores_foreign_epochs() -> Result<()> {
    for chunk in [1, 13, usize::MAX] {
        let mut c = replay(chunk, false);
        let mut cursor = 0;
        let anchor = Boundary::parse(line(&mut c, Source::Guest(1), &mut cursor, ANCHOR)?)?;
        let first = window(&mut c, 1, &mut cursor, anchor)?;
        let second = window(&mut c, 1, &mut cursor, first.end)?;
        assert_eq!(
            (first.peak, second.peak, second.end.missed - anchor.missed),
            (3, 10, 1)
        );
    }
    Ok(())
}
#[test]
fn missing_window_fails_instead_of_silently_scoring_a_longer_interval() -> Result<()> {
    let mut c = replay(11, true);
    let mut cursor = 0;
    let anchor = Boundary::parse(line(&mut c, Source::Guest(1), &mut cursor, ANCHOR)?)?;
    assert!(window(&mut c, 1, &mut cursor, anchor).is_err());
    Ok(())
}
