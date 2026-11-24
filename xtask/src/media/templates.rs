//! Staged, immutable template bytes for Linux images and static firmware ELFs.
//! Ownership/Lock Ordering: one launcher owns the staging directory; no VM or device locks.
//! Invariants: validate the original file, deduplicate only equal kind, content and RAM span.
use super::*;
use sha2::{Digest, Sha256};
#[derive(Copy, Clone, PartialEq, Eq)]
enum Kind {
    Kernel,
    Firmware,
    Initrd,
}
type ImageRange = (u64, u64, u64); // retained address, file bytes, loaded bytes
pub(super) struct Templates {
    pub(super) end: u64,
    sources: Vec<(Kind, [u8; 32], ImageRange)>,
    pub(super) loaders: Vec<Loader>,
}
impl Templates {
    pub(super) fn new(base: u64) -> Self {
        Self {
            end: base,
            sources: Vec::new(),
            loaders: Vec::new(),
        }
    }
    pub(super) fn stage(
        &mut self,
        scratch: &mut Scratch,
        vm: &mut Vm,
        defaults: &Defaults<'_>,
    ) -> Result<()> {
        let (address, len, memory) = if let Some(input) = &vm.firmware {
            ensure!(
                vm.kernel.is_none() && vm.initrd.is_none() && vm.probe.is_none(),
                "firmware VM cannot specify kernel, initramfs or Linux probe"
            );
            let elf = checked_bytes(input, MAX_KERNEL_BYTES)?;
            if input.sha256.is_some() {
                verify(input, Sha256::digest(&elf).into())?;
            }
            let (flat, size, layout) = crate::elf::firmware::load(&elf, vm.config.guest_base)
                .with_context(|| format!("firmware {}", input.path.display()))?;
            vm.config.firmware = Some(layout);
            self.retain(
                scratch,
                Kind::Firmware,
                &flat,
                size,
                Sha256::digest(&flat).into(),
            )?
        } else {
            let kernel = vm.kernel.as_ref().or(defaults.kernel).with_context(|| {
                format!("VM {} needs a kernel or firmware ELF", vm.config.name())
            })?;
            self.image(scratch, kernel, Kind::Kernel)?
        };
        vm.config.image.kernel_source = address;
        vm.config.image.kernel_size = len;
        vm.config.image.kernel_memory_size = memory;
        let initrd = match vm.probe {
            Some(mode) => {
                let elf = defaults.probe.context("missing compiled Linux probe")?;
                let bytes = crate::smoke::archive(elf, mode);
                Some(self.retain(
                    scratch,
                    Kind::Initrd,
                    &bytes,
                    bytes.len() as u64,
                    Sha256::digest(&bytes).into(),
                )?)
            }
            None if vm.firmware.is_some() => None, // Linux defaults never leak into firmware VMs.
            None => vm
                .initrd
                .as_ref()
                .or(defaults.initrd)
                .map(|source| self.image(scratch, source, Kind::Initrd))
                .transpose()?,
        };
        if let Some((address, len, _)) = initrd {
            vm.config.image.initrd_source = address;
            vm.config.image.initrd_size = len;
        }
        Ok(())
    }
    fn image(&mut self, scratch: &mut Scratch, input: &Input, kind: Kind) -> Result<ImageRange> {
        let limit = if kind == Kind::Kernel {
            MAX_KERNEL_BYTES
        } else {
            MAX_INITRD_BYTES
        };
        let bytes = checked_bytes(input, limit)?;
        let digest = Sha256::digest(&bytes).into();
        verify(input, digest)?;
        let loaded = if kind == Kind::Kernel {
            pauli_config::linux_image::memory_size(&bytes, bytes.len() as u64).map_err(|_| {
                anyhow::anyhow!(
                    "expected a raw little-endian RV64 Linux Image with 2 MiB text offset: {}",
                    input.path.display()
                )
            })?
        } else {
            bytes.len() as u64
        };
        self.retain(scratch, kind, &bytes, loaded, digest)
    }
    fn retain(
        &mut self,
        scratch: &mut Scratch,
        kind: Kind,
        bytes: &[u8],
        loaded: u64,
        digest: [u8; 32],
    ) -> Result<ImageRange> {
        if let Some((_, _, range)) = self
            .sources
            .iter()
            .find(|(tag, hash, range)| *tag == kind && *hash == digest && range.2 == loaded)
        {
            return Ok(*range);
        }
        let start = self.end;
        self.end = align(address(start, bytes.len() as u64)?, PAGE)?;
        let range = (start, bytes.len() as u64, loaded);
        let path = scratch.bytes(&format!("source-{}.bin", self.sources.len()), bytes)?;
        self.loaders.push(Loader {
            path,
            address: start,
        });
        self.sources.push((kind, digest, range));
        Ok(range)
    }
}
fn checked_bytes(input: &Input, limit: u64) -> Result<Vec<u8>> {
    let file = File::open(&input.path).with_context(|| format!("open {}", input.path.display()))?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && (1..=limit).contains(&meta.len()),
        "invalid input size: {}",
        input.path.display()
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 == meta.len(), "image length changed");
    Ok(bytes)
}
