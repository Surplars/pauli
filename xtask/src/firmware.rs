//! Diskless example profile; artifacts are explicit local inputs, never built via the RTOS.
//! Ownership/Lock Ordering: host staging owns a snapshot; no device or guest locks.
//! Invariants: one vCPU each, no storage/network, identical GPAs but distinct private RAM.
use crate::{
    args::Options,
    config::{Disk, Input, Vm},
};
use anyhow::{Context, Result};
use pauli_config::{CpuAffinity, VmConfig};
use std::path::Path;

pub fn pair(options: &Options, host_cpus: u8) -> Result<Vec<Vm>> {
    pair_inputs(options, "rtos-elf", host_cpus)
}

pub(crate) fn pair_inputs(options: &Options, control_key: &str, host_cpus: u8) -> Result<Vec<Vm>> {
    let elf = |key| {
        options
            .value(key)
            .filter(|path| !path.is_empty())
            .map(Path::new)
            .with_context(|| format!("firmware pair needs --{key} ELF"))
    };
    let vms = pair_files(elf(control_key)?, elf("service-elf")?, host_cpus)?;
    if host_cpus > 1 {
        println!("firmware placement: control=boot owner, service=non-boot worker");
    }
    Ok(vms)
}

pub(crate) fn pair_files(control: &Path, service: &Path, host_cpus: u8) -> Result<Vec<Vm>> {
    // Slot 0 with automatic placement follows whichever hart OpenSBI selected.
    // When there is a worker, exclude that management hart from service placement.
    let service_owner = if host_cpus > 1 {
        CpuAffinity::Workers
    } else {
        CpuAffinity::Automatic
    };
    let mut pair = Vec::new();
    for (name, path, kib, affinity) in [
        ("control", control, 256, CpuAffinity::Automatic),
        ("service", service, 32, service_owner),
    ] {
        let mut config = VmConfig::new(name).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        config.guest_base = 0x8020_0000; // Linked address of the example BSP, not a host reservation.
        config.memory_size = kib * 1024;
        config.autostart = true;
        config.cpu_affinity = affinity;
        pair.push(Vm {
            config,
            firmware: Some(Input::local(path)),
            kernel: None,
            initrd: None,
            disk: None,
            disk_mode: Disk::None,
            bootargs: String::new(),
            probe: None,
        });
    }
    Ok(pair)
}
