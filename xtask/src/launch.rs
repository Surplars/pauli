//! Translate explicit VM/backend selections into a QEMU virt command.
//! Ownership/Lock Ordering: scratch outlives the child; supplied raw disks aren't scratch.
//! Invariants: no shell, no implicit network, no writes to a raw disk without --persistent.
use crate::{
    args::Options,
    build,
    config::{self, Board, Input, Vm},
    media::{self, Defaults, Media, Scratch},
    tty,
};
use anyhow::{Context, Result, ensure};
use pauli_config::MAX_DISKS;
use std::{
    path::{Path, PathBuf},
    process::Command,
};
const CPU: &str = "rv64i,m=true,a=true,c=true,v=false,s=true,u=true,h=true,mmu=true,pmp=true,sv39=true,zicsr=true,zifencei=true,zicntr=true";
#[cfg(test)]
mod tests;
pub fn run(root: &Path, board: &Board, options: &Options) -> Result<()> {
    options.only(&[
        "board",
        "vm",
        "kernel",
        "initrd",
        "host-cpus",
        "host-memory-mib",
        "disk",
        "persistent",
        "features",
        "aia-host",
        "dry-run",
        "rtos-elf",
        "service-elf",
    ])?;
    let cpus = options.number("host-cpus", 1, u64::from(board.max_cpus))? as u8;
    let (mut vms, managed) = if options.flag("rtos-elf") || options.flag("service-elf") {
        ensure!(
            !["vm", "kernel", "initrd", "disk", "persistent"]
                .iter()
                .any(|k| options.flag(k)),
            "the diskless firmware pair does not take a profile or extra media"
        );
        (crate::firmware::pair(options, cpus)?, Vec::new())
    } else {
        config::profile(Path::new(
            options.value("vm").context("--vm PROFILE is required")?,
        ))?
    };
    let platform = Platform::new(board, options, &vms, Mode::Normal)?;
    let defaults = [
        input(options, "kernel"),
        input(options, "initrd"),
        input(options, "disk"),
    ];
    let mut scratch = Scratch::new(root)?;
    let media = media::prepare(
        &mut scratch,
        board,
        &mut vms,
        Defaults {
            host_mib: options
                .value("host-memory-mib")
                .map(|_| options.number("host-memory-mib", 1, (u64::MAX - board.ram_base) >> 20))
                .transpose()?,
            kernel: defaults[0].as_ref(),
            initrd: defaults[1].as_ref(),
            disk: defaults[2].as_ref(),
            managed: &managed,
            probe: None,
        },
        cpus,
    )?;
    if defaults[2].is_some() {
        ensure!(
            vms.iter()
                .any(|vm| vm.config.disk.is_some() && vm.disk.is_none()),
            "unused --disk"
        );
    }
    let elf = platform.build(root)?;
    let mut command = platform.command(&elf, cpus, Some(&media), options.flag("persistent"))?;
    println!("launch: {command:?}");
    if options.flag("dry-run") {
        Ok(())
    } else {
        tty::run(&mut command)
    }
}
pub fn input(options: &Options, name: &str) -> Option<Input> {
    options
        .value(name)
        .map(|path| Input::local(Path::new(path)))
}
/// Observing a guest workload compiles counters, but must not replace that
/// workload with the board's boot-time self-test or alter its advertised ISA.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Observed,
    HvSelftest,
}

pub struct Platform<'a> {
    board: &'a Board,
    aia: bool,
    mode: Mode,
    fd: bool,
    features: String,
}
impl<'a> Platform<'a> {
    pub fn new(board: &'a Board, options: &Options, vms: &[Vm], mode: Mode) -> Result<Self> {
        ensure!(
            board.qemu,
            "this board supports build/image, not QEMU launch"
        );
        let aia = options.flag("aia-host");
        let mut features: Vec<&str> = options
            .value("features")
            .map(|set| set.split(',').collect())
            .unwrap_or_default();
        if aia && !features.contains(&"aia") {
            features.push("aia");
        }
        if mode != Mode::Normal && !features.contains(&"selftest") {
            features.push("selftest");
        }
        ensure!(
            aia || !vms.iter().any(|vm| vm.config.aia),
            "AIA guest profile requires --aia-host"
        );
        ensure!(
            aia || !features.contains(&"aia-guest-file"),
            "aia-guest-file requires --aia-host"
        );
        Ok(Self {
            board,
            aia,
            mode,
            fd: mode == Mode::HvSelftest || vms.iter().any(|vm| vm.firmware.is_none()),
            features: features.join(","),
        })
    }
    pub fn build(&self, root: &Path) -> Result<PathBuf> {
        build::board(
            root,
            self.board,
            (!self.features.is_empty()).then_some(self.features.as_str()),
        )
    }
    pub fn command(
        &self,
        elf: &Path,
        cpus: u8,
        media: Option<&Media>,
        persistent: bool,
    ) -> Result<Command> {
        let mut cmd = self.machine(elf, cpus, media.map_or(256, |data| data.ram_mib));
        if self.mode == Mode::HvSelftest {
            cmd.args(["-append", "pauli.selftest"]);
        }
        if let Some(media) = media {
            devices(&mut cmd, media, persistent)?;
        }
        Ok(cmd)
    }
    /// Keep the emulator/firmware environment identical when comparing with a
    /// direct OpenSBI payload; only the VMM launch adds loaders and guest media.
    pub(crate) fn machine(&self, elf: &Path, cpus: u8, ram_mib: u64) -> Command {
        let mut cmd = Command::new(
            std::env::var_os("PAULI_QEMU").or_else(|| std::env::var_os("OPENION_QEMU")).unwrap_or_else(|| "qemu-system-riscv64".into()),
        );
        let machine = if !self.aia {
            "virt"
        } else if self
            .features
            .split(',')
            .any(|name| name == "aia-guest-file")
        {
            "virt,aia=aplic-imsic,aia-guests=1"
        } else {
            "virt,aia=aplic-imsic"
        };
        cmd.args([
            "-machine",
            machine,
            "-cpu",
            &format!("{CPU},f={0},d={0}", self.fd),
            "-smp",
            &cpus.to_string(),
            "-m",
            &format!("{ram_mib}M"),
            "-bios",
            "default",
            "-kernel",
        ])
        .arg(elf)
        .args([
            "-display",
            "none",
            "-serial",
            "stdio",
            "-monitor",
            "none",
            "-no-reboot",
            "-nic",
            "none",
            "-rtc",
            "base=utc,clock=host",
            "-global",
            "virtio-mmio.force-legacy=false",
        ]);
        cmd
    }
}
fn devices(cmd: &mut Command, media: &Media, persistent: bool) -> Result<()> {
    for loader in &media.loaders {
        cmd.args([
            "-device",
            &format!(
                "loader,file={},addr={:#x},force-raw=on",
                property(&loader.path)?,
                loader.address
            ),
        ]);
    }
    for (slot, disk) in media.disks.iter().enumerate() {
        let mode = if !disk.config.writable {
            ",readonly=on"
        } else if !persistent {
            ",snapshot=on"
        } else {
            ""
        };
        cmd.args([
            "-drive",
            &format!(
                "if=none,id=disk{slot},format=raw,file={}{}",
                property(&disk.path)?,
                mode
            ),
            "-device",
            &format!("virtio-blk-device,bus=virtio-mmio-bus.{slot},drive=disk{slot}"),
        ]);
    }
    if media.uplink {
        cmd.args([
            "-netdev",
            "user,id=uplink",
            "-device",
            &format!("virtio-net-device,bus=virtio-mmio-bus.{MAX_DISKS},netdev=uplink"),
        ]);
    }
    Ok(())
}
fn property(path: &Path) -> Result<String> {
    let path = path.to_str().context("QEMU image path is not UTF-8")?;
    ensure!(
        !path.contains(['\n', '\r', '\0']),
        "invalid QEMU image path"
    );
    Ok(path.replace(',', ",,")) // QEMU keyval escaping, not shell escaping
}
