//! Resolve host raw disks to private or intentionally shared read-only slots.
//! Ownership/Lock Ordering: open each file read-only for metadata/hash; QEMU owns I/O.
//! Invariants: a writer excludes every alias, including symlinks and hard links.
use crate::{
    config::{Disk, Input, Vm},
    media,
};
use anyhow::{Context, Result, ensure};
use pauli_config::{DiskConfig, MAX_DISKS};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, Metadata},
    io::Read,
    path::PathBuf,
};

const SECTOR: u64 = 512;
const MAX_BYTES: u64 = 128 * 1024 * 1024 * 1024;
pub struct DiskBinding {
    pub path: PathBuf,
    pub config: DiskConfig,
    identity: (u64, u64),
}
pub fn bind(vm: &mut Vm, default: Option<&Input>, disks: &mut Vec<DiskBinding>) -> Result<()> {
    if vm.disk_mode == Disk::None {
        ensure!(
            vm.disk.is_none(),
            "disk_image specified for VM with disk=none"
        );
        return Ok(());
    }
    let source = vm
        .disk
        .as_ref()
        .or(default)
        .with_context(|| format!("VM {} needs --disk RAW or disk_image", vm.config.name()))?;
    let writable = vm.disk_mode == Disk::Writable;
    vm.config.disk = Some(register(source, writable, false, disks)?);
    vm.config.writable = writable;
    Ok(())
}
/// Managed volumes are exclusive even in read-only mode. Two host paths to the
/// same file must never create two independently grantable capabilities.
pub fn register(
    source: &Input,
    writable: bool,
    managed: bool,
    disks: &mut Vec<DiskBinding>,
) -> Result<u8> {
    let path = fs::canonicalize(&source.path)
        .with_context(|| format!("open {}", source.path.display()))?;
    let file = File::open(&path)?;
    let meta = file.metadata()?;
    let bytes = meta.len();
    ensure!(
        meta.is_file() && (SECTOR..=MAX_BYTES).contains(&bytes) && bytes % SECTOR == 0,
        "expected a sector-aligned raw disk of at most 128 GiB"
    );
    if source.sha256.is_some() {
        hash(source, file, bytes)?;
    }
    let identity = identity(&meta)?;
    let slot = match disks.iter().position(|disk| disk.identity == identity) {
        Some(slot) => {
            ensure!(
                !managed
                    && !disks[slot].config.managed
                    && !writable
                    && !disks[slot].config.writable,
                "exclusive disk alias: {}",
                source.path.display()
            );
            slot
        }
        None => {
            let slot = disks.len();
            ensure!(slot < MAX_DISKS, "too many native disks");
            disks.push(DiskBinding {
                path,
                identity,
                config: DiskConfig {
                    sectors: bytes / SECTOR,
                    writable,
                    managed,
                },
            });
            slot
        }
    };
    Ok(slot as u8)
}
fn hash(source: &Input, mut file: File, size: u64) -> Result<()> {
    let mut remaining = size;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    while remaining != 0 {
        let count = remaining.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..count])?;
        digest.update(&buffer[..count]);
        remaining -= count as u64;
    }
    ensure!(file.metadata()?.len() == size, "disk length changed");
    media::verify(source, digest.finalize().into())
}
fn identity(meta: &Metadata) -> Result<(u64, u64)> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        Ok((
            u64::from(
                meta.volume_serial_number()
                    .context("missing disk volume identity")?,
            ),
            meta.file_index().context("missing disk file identity")?,
        ))
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok((meta.dev(), meta.ino()))
    }
}
