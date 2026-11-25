//! Retain checked image bytes for one launch and lay out host-only loader ranges.
//! Ownership/Lock Ordering: scratch owns only new files; no shared output paths.
//! Invariants: immutable sources, metadata, private VM RAM and firmware space are disjoint.
use crate::{
    config::{Board, Input, ManagedDisk, Vm},
    disk::{DiskBinding, bind},
    dtb, work,
};
use anyhow::{Context, Result, ensure};
use pauli_config::{
    ALIGN, BootConfig, GuestBoardLayout, MAX_BOOT_VMS, MAX_DISKS, MAX_INITRD_BYTES,
    MAX_KERNEL_BYTES, WIRE_SIZE,
    loader::{LEGACY_RAM, metadata_offsets},
};
mod templates;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use templates::Templates;

const MIB: u64 = 1024 * 1024;
const PAGE: u64 = 4096;
const NATIVE_TAIL: u64 = 16 * MIB;
// Diskless firmware needs no DMA pool. Keep the top clear for QEMU/OpenSBI's
// native DTB; the native loader also allocates in the gaps below the templates.
const FIRMWARE_TAIL: u64 = 4 * MIB;
pub(crate) const FIRMWARE_RAM_MIB: u64 = 32;

pub struct Scratch {
    directory: PathBuf,
    files: Vec<PathBuf>,
}
impl Scratch {
    pub fn new(root: &Path) -> Result<Self> {
        let parent = work(root).join("hv-run");
        fs::create_dir_all(&parent)?;
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        // Concurrent fixtures can observe the same Windows clock tick.
        static SERIAL: AtomicUsize = AtomicUsize::new(0);
        let seq = SERIAL.fetch_add(1, Ordering::Relaxed);
        let directory = parent.join(format!("{}-{stamp}-{seq}", std::process::id()));
        fs::create_dir(&directory)?;
        Ok(Self {
            directory,
            files: Vec::new(),
        })
    }
    pub fn file(&mut self, name: &str) -> Result<(PathBuf, File)> {
        // Only compiled-in names and numeric indices are passed by the launcher.
        let path = self.directory.join(name);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        self.files.push(path.clone());
        Ok((path, file))
    }
    pub(crate) fn bytes(&mut self, name: &str, bytes: &[u8]) -> Result<PathBuf> {
        let (path, mut file) = self.file(name)?;
        file.write_all(bytes)?;
        Ok(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        for path in &self.files {
            if let Err(error) = fs::remove_file(path) {
                eprintln!(
                    "could not remove generated file {}: {error}",
                    path.display()
                );
            }
        }
        if let Err(error) = fs::remove_dir(&self.directory) {
            eprintln!(
                "could not remove scratch directory {}: {error}",
                self.directory.display()
            );
        }
    }
}

pub struct Loader {
    pub path: PathBuf,
    pub address: u64,
}
pub struct Media {
    pub loaders: Vec<Loader>,
    pub disks: Vec<DiskBinding>,
    pub uplink: bool,
    pub ram_mib: u64,
}
pub struct Defaults<'a> {
    pub host_mib: Option<u64>,
    pub kernel: Option<&'a Input>,
    pub initrd: Option<&'a Input>,
    pub disk: Option<&'a Input>,
    pub managed: &'a [ManagedDisk],
    pub probe: Option<&'a [u8]>,
}
pub fn prepare(
    scratch: &mut Scratch,
    board: &Board,
    vms: &mut [Vm],
    defaults: Defaults<'_>,
    host_cpus: u8,
) -> Result<Media> {
    let (floor, tail) = if vms.iter().all(|vm| vm.firmware.is_some()) {
        (FIRMWARE_RAM_MIB * MIB, FIRMWARE_TAIL)
    } else {
        (LEGACY_RAM, NATIVE_TAIL)
    };
    let requested = defaults
        .host_mib
        .map(|n| n.checked_mul(MIB).context("host RAM overflow"))
        .transpose()?;
    let (dtb_offset, config_offset) = metadata_offsets(requested.unwrap_or(floor).max(floor));
    let dtbs_at = address(board.ram_base, dtb_offset)?;
    let manifest_at = address(board.ram_base, config_offset)?;
    let template_base = align(address(manifest_at, WIRE_SIZE as u64)?, ALIGN)?;
    let mut templates = Templates::new(template_base);
    let mut disks = Vec::new();
    for vm in vms.iter_mut() {
        templates.stage(scratch, vm, &defaults)?;
        bind(vm, defaults.disk, &mut disks)?;
        wire(vm.config.validate_placement(host_cpus))
            .with_context(|| format!("VM {}", vm.config.name()))?;
    }
    for volume in defaults.managed {
        crate::disk::register(&volume.input, volume.writable, true, &mut disks)?;
    }
    let (vms_top, mut config) = place(board, vms, dtbs_at, template_base, templates.end, &disks)?;
    if config.vms.iter().flatten().any(|vm| vm.exclusive_cpus) {
        // OpenSBI chooses the boot hart. Reject a profile now if no choice can work.
        let count = usize::from(host_cpus);
        ensure!(
            (0..count).any(|boot| config.exclusive_owner_mask(boot, count).is_ok()),
            "exclusive CPU placement leaves no valid topology for {count} host CPUs"
        );
    }
    let dtbs = build_dtbs(&mut config, vms, board.hz)?;
    let bytes = wire(config.encode())?;
    for (name, bytes, address) in [
        ("guest.dtb", dtbs.as_slice(), dtbs_at),
        ("boot.bin", &bytes, manifest_at),
    ] {
        templates.loaders.push(Loader {
            path: scratch.bytes(name, bytes)?,
            address,
        });
    }
    let occupied = vms_top.max(templates.end) - board.ram_base;
    let minimum = align(address(occupied, tail)?.max(floor), MIB)?;
    let ram = requested.unwrap_or(minimum);
    ensure!(
        ram >= minimum,
        "--host-memory-mib below staged minimum of {} MiB",
        minimum / MIB
    );
    let ram_mib = ram / MIB;
    for vm in config
        .vms
        .iter()
        .flatten()
        .filter(|vm| vm.firmware.is_some())
    {
        println!(
            "firmware {}: load={} B, linked RAM span={} B, private={} KiB",
            vm.name(),
            vm.image.kernel_size,
            vm.image.kernel_memory_size,
            vm.memory_size >> 10
        );
    }
    Ok(Media {
        loaders: templates.loaders,
        disks,
        uplink: vms.iter().any(|vm| vm.config.net_uplink),
        ram_mib,
    })
}

fn place(
    board: &Board,
    vms: &mut [Vm],
    dtbs: u64,
    base: u64,
    end: u64,
    disks: &[DiskBinding],
) -> Result<(u64, BootConfig)> {
    ensure!(
        (1..=MAX_BOOT_VMS).contains(&vms.len()) && disks.len() <= MAX_DISKS,
        "profile capacity exceeded"
    );
    let mut low = align(address(board.entry, board.limit)?, ALIGN)?;
    let mut high = align(end, ALIGN)?;
    let mut config = BootConfig {
        template_base: base,
        template_size: end - base,
        dtb_size: vms.len() as u64, // One placeholder byte per DTB before final encoding.
        management: true,
        smoke_payload: false,
        disks: [None; MAX_DISKS],
        vms: [None; MAX_BOOT_VMS],
    };
    for (index, vm) in vms.iter_mut().enumerate() {
        // Keep small VMs below the fixed metadata windows; large ones start after the templates.
        let size = vm.config.memory_size;
        let alignment = if vm.config.firmware.is_some() {
            PAGE
        } else {
            ALIGN
        };
        let cursor = if address(align(low, alignment)?, size)? <= dtbs {
            &mut low
        } else {
            &mut high
        };
        vm.config.host_base = align(*cursor, alignment)?;
        *cursor = address(vm.config.host_base, size)?;
        // Validate image/RAM/placement before computing derived guest addresses in DTB.
        vm.config.dtb_offset = index as u64;
        vm.config.dtb_size = 1;
        config.vms[index] = Some(vm.config);
    }
    for (slot, disk) in config.disks.iter_mut().zip(disks) {
        *slot = Some(disk.config);
    }
    wire(config.validate())?;
    Ok((low.max(high), config))
}

fn build_dtbs(config: &mut BootConfig, vms: &[Vm], hz: u32) -> Result<Vec<u8>> {
    let mut dtbs = Vec::new();
    for (slot, vm) in config.vms.iter_mut().flatten().zip(vms) {
        let bytes = dtb::guest(slot, GuestBoardLayout::DEFAULT, hz, &vm.bootargs)?;
        slot.dtb_offset = dtbs.len() as u64;
        slot.dtb_size = bytes.len() as u64;
        dtbs.extend_from_slice(&bytes);
        dtbs.resize(dtbs.len().next_multiple_of(8), 0);
    }
    config.dtb_size = dtbs.len() as u64;
    // encode() revalidates the final manifest including the real DTB lengths.
    Ok(dtbs)
}

pub fn verify(input: &Input, digest: [u8; 32]) -> Result<()> {
    if let Some(expected) = &input.sha256 {
        let actual: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        ensure!(
            &actual == expected,
            "SHA-256 mismatch for {}",
            input.path.display()
        );
    }
    Ok(())
}
fn wire<T>(result: std::result::Result<T, pauli_config::Error>) -> Result<T> {
    result.map_err(|error| anyhow::anyhow!("invalid boot manifest: {error:?}"))
}
fn align(value: u64, alignment: u64) -> Result<u64> {
    Ok(address(value, alignment - 1)? & !(alignment - 1))
}
fn address(base: u64, size: u64) -> Result<u64> {
    base.checked_add(size).context("loader address overflow")
}

#[cfg(test)]
mod tests;
