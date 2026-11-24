//! Parse checked board contracts and VM manifests before producing loader files.
//! Ownership/Lock Ordering: host-only configuration; nothing is published to the VMM yet.
//! Invariants: paths are relative to their profile; duplicate IDs and unknown keys fail.
use anyhow::{Context, Result, bail, ensure};
use pauli_config::{CpuAffinity, MAX_BOOT_VMS, VmConfig};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};
use toml::{Table, Value};
pub struct Board {
    pub name: String,
    pub target: String,
    pub ram_base: u64,
    pub entry: u64,
    pub limit: u64,
    pub hz: u32,
    pub max_cpus: u8,
    pub qemu: bool,
}
impl Board {
    pub fn load(root: &Path, name: &str) -> Result<Self> {
        ensure!(
            matches!(name, "qemu-virt-riscv" | "valence-wave8-ooo"),
            "unknown board {name}"
        );
        let path = root.join("configs/boards").join(format!("{name}.toml"));
        let table = read(&path)?;
        keys(
            &table,
            &[
                "target",
                "ram_base",
                "payload_entry",
                "payload_limit",
                "timebase_hz",
                "max_cpus",
                "qemu",
            ],
        )?;
        let board = Self {
            name: name.to_owned(),
            target: text(&table, "target")?
                .context("board target is required")?
                .to_owned(),
            ram_base: number(&table, "ram_base", 0)?,
            entry: number(&table, "payload_entry", 0)?,
            limit: number(&table, "payload_limit", 0)?,
            hz: u32::try_from(number(&table, "timebase_hz", 0)?)?,
            max_cpus: u8::try_from(number(&table, "max_cpus", 0)?)?,
            qemu: boolean(&table, "qemu", false)?,
        };
        ensure!(
            board.target == "riscv64imac-unknown-none-elf"
                && board.hz != 0
                && (1..=pauli_config::MAX_HOST_CPUS).contains(&board.max_cpus)
                && board.entry >= board.ram_base
                && board.limit > 0
                && board.entry.checked_add(board.limit).is_some(),
            "invalid board contract"
        );
        Ok(board)
    }
}
#[derive(Clone)]
pub struct Input {
    pub path: PathBuf,
    pub sha256: Option<String>,
}
impl Input {
    pub fn local(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            sha256: None,
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Disk {
    None,
    ReadOnly,
    Writable,
}
pub struct Vm {
    pub config: VmConfig,
    pub firmware: Option<Input>,
    pub bootargs: String,
    pub kernel: Option<Input>,
    pub initrd: Option<Input>,
    pub disk: Option<Input>,
    pub disk_mode: Disk,
    pub probe: Option<u8>,
}
impl Vm {
    pub fn diskless(name: &str) -> Result<Self> {
        let mut config = VmConfig::new(name).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        config.autostart = true;
        Ok(Self {
            config,
            firmware: None,
            bootargs: String::new(),
            kernel: None,
            initrd: None,
            disk: None,
            disk_mode: Disk::None,
            probe: None,
        })
    }
    pub fn probe(name: &str, mode: u8) -> Result<Self> {
        let mut vm = Self::diskless(name)?;
        vm.bootargs = "console=ttyS0,115200 rdinit=/init loglevel=4 iomem=relaxed".into();
        vm.probe = Some(mode);
        Ok(vm)
    }
}
pub struct ManagedDisk {
    pub input: Input,
    pub writable: bool,
}
pub fn profile(path: &Path) -> Result<(Vec<Vm>, Vec<ManagedDisk>)> {
    let root = read(path)?;
    keys(&root, &["vm", "managed_disk"])?;
    let entries = root
        .get("vm")
        .and_then(Value::as_array)
        .context("expected [[vm]] entries")?;
    ensure!(
        (1..=MAX_BOOT_VMS).contains(&entries.len()),
        "expected 1..={MAX_BOOT_VMS} boot VMs"
    );
    let directory = path.parent().unwrap_or(Path::new("."));
    let mut peers = Vec::new();
    let mut vms = Vec::new();
    for entry in entries {
        let table = entry.as_table().context("each VM must be a table")?;
        let vm = parse_vm(table, directory)?;
        ensure!(
            !vms.iter()
                .any(|other: &Vm| other.config.name() == vm.config.name()),
            "duplicate VM {}",
            vm.config.name()
        );
        vms.push(vm);
        peers.push(text(table, "net_peer")?);
    }
    for (slot, peer) in peers.into_iter().enumerate() {
        if let Some(peer) = peer {
            let target = vms
                .iter()
                .position(|vm| vm.config.name() == peer)
                .context("unknown network peer")?;
            vms[slot].config.net_peer = Some(target as u8);
        }
    }
    let mut disks = Vec::new();
    if let Some(entries) = root.get("managed_disk") {
        let entries = entries
            .as_array()
            .context("expected [[managed_disk]] entries")?;
        ensure!(
            entries.len() <= pauli_config::MAX_DISKS,
            "too many managed disks"
        );
        for entry in entries {
            let table = entry
                .as_table()
                .context("each managed disk must be a table")?;
            keys(table, &["image", "sha256", "mode"])?;
            let input = source(table, "image", "sha256", directory)?
                .context("managed disk needs image and sha256")?;
            let writable = match text(table, "mode")? {
                Some("rw") => true,
                Some("ro") => false,
                _ => bail!("managed disk needs explicit mode ro or rw"),
            };
            disks.push(ManagedDisk { input, writable });
        }
    }
    Ok((vms, disks))
}
fn parse_vm(table: &Table, dir: &Path) -> Result<Vm> {
    keys(
        table,
        &[
            "name",
            "memory_mib",
            "memory_kib",
            "firmware",
            "firmware_sha256",
            "guest_base",
            "quantum_us",
            "autostart",
            "disk",
            "net_peer",
            "net_uplink",
            "reset",
            "interrupts",
            "vcpus",
            "cpu_affinity",
            "bootargs",
            "kernel",
            "kernel_sha256",
            "initramfs",
            "initramfs_sha256",
            "disk_image",
            "disk_sha256",
        ],
    )?;
    let name = text(table, "name")?.context("VM name is required")?;
    let mut config = VmConfig::new(name).map_err(|err| anyhow::anyhow!("VM {name}: {err:?}"))?;
    let firmware = source(table, "firmware", "firmware_sha256", dir)?;
    config.memory_size = if table.contains_key("memory_kib") {
        ensure!(
            firmware.is_some() && !table.contains_key("memory_mib"),
            "memory_kib requires firmware and excludes memory_mib"
        );
        number(table, "memory_kib", 0)?.checked_mul(1024)
    } else {
        number(table, "memory_mib", if firmware.is_some() { 1 } else { 96 })?
            .checked_mul(1024 * 1024)
    }
    .context("guest RAM size overflow")?;
    config.guest_base = number(table, "guest_base", config.guest_base)?;
    config.quantum_us = number(table, "quantum_us", config.quantum_us)?;
    config.vcpus = u8::try_from(number(table, "vcpus", 1)?)?;
    config.cpu_affinity = affinity(table.get("cpu_affinity"), config.vcpus)?;
    config.autostart = boolean(table, "autostart", false)?;
    config.net_uplink = boolean(table, "net_uplink", false)?;
    config.stop_on_reset = match text(table, "reset")?.unwrap_or("reload") {
        "reload" => false,
        "stop" => true,
        _ => bail!("VM reset must be reload or stop"),
    };
    config.aia = match text(table, "interrupts")?.unwrap_or("plic") {
        "plic" => false,
        "aia" => true,
        _ => bail!("VM interrupts must be plic or aia"),
    };
    let disk_mode = match text(table, "disk")?.unwrap_or("none") {
        "none" => Disk::None,
        "ro" => Disk::ReadOnly,
        "rw" => Disk::Writable,
        _ => bail!("VM disk must be none, ro or rw"),
    };
    let bootargs = text(table, "bootargs")?
        .unwrap_or("console=ttyS0,115200")
        .to_owned();
    ensure!(
        bootargs.len() <= 2048 && !bootargs.as_bytes().contains(&0),
        "invalid kernel command line"
    );
    Ok(Vm {
        config,
        firmware,
        bootargs,
        disk_mode,
        probe: None,
        kernel: source(table, "kernel", "kernel_sha256", dir)?,
        initrd: source(table, "initramfs", "initramfs_sha256", dir)?,
        disk: source(table, "disk_image", "disk_sha256", dir)?,
    })
}
fn affinity(value: Option<&Value>, cpus: u8) -> Result<CpuAffinity> {
    match value {
        None => Ok(CpuAffinity::Automatic),
        Some(Value::String(mode)) if mode == "automatic" || mode == "auto" => {
            Ok(CpuAffinity::Automatic)
        }
        Some(Value::String(mode)) if mode == "workers" => Ok(CpuAffinity::Workers),
        Some(Value::Array(list)) => {
            ensure!(
                list.len() == cpus as usize
                    && list.len() <= usize::from(pauli_config::MAX_GUEST_VCPUS),
                "cpu_affinity length must equal vcpus"
            );
            let mut owners = [0; pauli_config::MAX_GUEST_VCPUS as usize];
            for (index, entry) in list.iter().enumerate() {
                owners[index] = u8::try_from(entry.as_integer().context("invalid CPU affinity")?)?;
            }
            Ok(CpuAffinity::Physical(owners))
        }
        _ => bail!("cpu_affinity must be auto, workers or an array"),
    }
}
fn source(table: &Table, key: &str, hash: &str, dir: &Path) -> Result<Option<Input>> {
    let (path, digest) = (text(table, key)?, text(table, hash)?);
    match (path, digest) {
        (None, None) => Ok(None),
        (Some(path), Some(digest))
            if !path.is_empty()
                && digest.len() == 64
                && digest.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            Ok(Some(Input {
                path: dir.join(path),
                sha256: Some(digest.to_ascii_lowercase()),
            }))
        }
        _ => bail!("{key} in TOML requires {hash} (64 hex digits)"),
    }
}
fn read(path: &Path) -> Result<Table> {
    let mut body = String::new();
    File::open(path)
        .with_context(|| format!("open {}", path.display()))?
        .take(65_537)
        .read_to_string(&mut body)?;
    ensure!(body.len() <= 65_536, "TOML profile exceeds 64 KiB");
    body.parse::<Table>()
        .with_context(|| format!("parse {}", path.display()))
}
fn keys(table: &Table, allowed: &[&str]) -> Result<()> {
    for key in table.keys() {
        ensure!(allowed.contains(&key.as_str()), "unknown profile key {key}");
    }
    Ok(())
}
fn text<'a>(table: &'a Table, name: &str) -> Result<Option<&'a str>> {
    table
        .get(name)
        .map(|value| {
            value
                .as_str()
                .with_context(|| format!("{name} must be a string"))
        })
        .transpose()
}
fn number(table: &Table, name: &str, default: u64) -> Result<u64> {
    let Some(value) = table.get(name) else {
        return Ok(default);
    };
    Ok(u64::try_from(
        value
            .as_integer()
            .with_context(|| format!("{name} must be an integer"))?,
    )?)
}
fn boolean(table: &Table, name: &str, default: bool) -> Result<bool> {
    table.get(name).map_or(Ok(default), |v| {
        v.as_bool()
            .with_context(|| format!("{name} must be a boolean"))
    })
}
