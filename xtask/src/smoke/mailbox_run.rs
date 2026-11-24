//! Launch one two-vCPU bare VS guest through the normal checked RV64 Image loader.
//! Scratch owns all inputs except the read-only host ELF. No Linux, disks or network.
use super::{
    mailbox::{self, Workload},
    verify_process,
};
use crate::{
    args::Options,
    build,
    config::{Board, Input, Vm},
    elf,
    launch::{Mode, Platform},
    media::{self, Defaults, Scratch},
};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

const VCPUS: u8 = 2;
const PRIVATE_BYTES: u64 = 64 << 20; // Minimum for the existing Image layout.

fn profile(host_cpus: u8) -> Result<Vm> {
    let mut vm = Vm::diskless(super::ALPHA)?;
    vm.config.vcpus = VCPUS;
    vm.config.memory_size = PRIVATE_BYTES;
    let mut pins = [0; pauli_config::MAX_GUEST_VCPUS as usize];
    pins[1] = host_cpus - 1; // Place the same two VS harts together or apart.
    vm.config.cpu_affinity = pauli_config::CpuAffinity::Physical(pins);
    Ok(vm)
}

/// Only the selftest host we build here is known to export exact fence tickets.
fn split_mode(features: Option<&str>, supplied: Option<&Path>) -> Result<bool> {
    match (features, supplied) {
        (None, _) => Ok(false),
        (Some("selftest"), None) => Ok(true),
        _ => anyhow::bail!("mailbox accepts only --features selftest, without --host-elf"),
    }
}

pub(super) fn run(root: &Path, board: &Board, options: &Options, suite: &str) -> Result<()> {
    let handoff = suite == "ipi-handoff";
    if handoff {
        // Handoff varies the guest's post-ack policy, never the host build.
        options.only(&["suite", "board", "timeout-secs", "host-cpus", "dry-run"])?;
    } else {
        options.only(&[
            "suite",
            "board",
            "timeout-secs",
            "host-elf",
            "host-cpus",
            "features",
            "dry-run",
        ])?;
    }
    ensure!(
        board.name == "qemu-virt-riscv",
        "mailbox uses the QEMU virt VS fixture"
    );
    let seconds = options.number("timeout-secs", 40, 120)?;
    let host_cpus = options.number("host-cpus", if handoff { 1 } else { 2 }, 2)? as u8;
    let supplied = options.value("host-elf").map(Path::new);
    let workload = if handoff {
        Workload::IpiHandoff
    } else if split_mode(options.value("features"), supplied)? {
        Workload::FenceSplit
    } else {
        Workload::RoundTrip
    };
    if let Some(path) = supplied {
        elf::inspect(path, board).context("invalid mailbox comparison host ELF")?;
    }
    let mut vms = [profile(host_cpus)?];
    let platform = Platform::new(board, options, &vms, Mode::Normal)?;
    let guest = build::board_example(root, board, "vs_mailbox")?;
    let (layout, flat) = elf::flat_image(&guest, board)?;
    ensure!(
        layout.entry == vms[0].config.guest_base + pauli_config::KERNEL_OFFSET,
        "mailbox link address differs from Image entry"
    );
    let mut scratch = Scratch::new(root)?;
    vms[0].kernel = Some(Input::local(&scratch.bytes("mailbox.Image", &flat)?));
    let media = media::prepare(
        &mut scratch,
        board,
        &mut vms,
        Defaults {
            host_mib: None,
            kernel: None,
            initrd: None,
            disk: None,
            managed: &[],
            probe: None,
        },
        host_cpus,
    )?;
    ensure!(
        vms[0].config.image.kernel_memory_size >= layout.ram_span,
        "mailbox Image header omits ELF-reserved RAM"
    );
    let host = match supplied {
        Some(path) => path.to_owned(),
        None => platform.build(root)?,
    };
    let command = platform.command(&host, host_cpus, Some(&media), false)?;
    println!(
        "mailbox inputs: host_sha256={:x} guest_image_sha256={:x} guest_bytes={} host_cpus={host_cpus}",
        Sha256::digest(fs::read(&host)?),
        Sha256::digest(&flat),
        flat.len()
    );
    println!("smoke {suite}: {command:?}");
    if !options.flag("dry-run") {
        verify_process(command, seconds, |c| {
            c.wait(super::Source::Host, 0, "hv> ")?;
            mailbox::verify(c, host_cpus, workload)?;
            c.shutdown()
        })?;
        println!("smoke {suite}: PASS (clean QEMU exit, wall deadline {seconds}s)");
    }
    Ok(())
}

#[cfg(test)]
#[test]
fn reference_hosts_never_request_diagnostic_records() {
    let supplied = Some(Path::new("outside.elf"));
    assert!(!split_mode(None, supplied).unwrap());
    assert!(split_mode(Some("selftest"), None).unwrap());
    assert!(split_mode(Some("selftest"), supplied).is_err());
    assert!(split_mode(Some("guest-control"), None).is_err());
}

#[cfg(test)]
#[test]
fn changing_host_count_changes_only_the_two_vcpu_placement() -> Result<()> {
    for (host_cpus, expected) in [(1, [0, 0]), (2, [0, 1])] {
        let vm = profile(host_cpus)?;
        let cfg = &vm.config;
        assert_eq!((cfg.vcpus, cfg.memory_size), (2, PRIVATE_BYTES));
        assert!(cfg.validate_placement(host_cpus).is_ok());
        for boot in 0..usize::from(host_cpus) {
            assert_eq!(
                [0, 1].map(|i| cfg.owner(0, i, boot, host_cpus.into())),
                expected
            );
        }
        assert!(vm.firmware.is_none() && vm.probe.is_none() && vm.initrd.is_none());
        assert!(vm.disk.is_none() && cfg.net_peer.is_none() && !cfg.net_uplink);
    }
    Ok(())
}
