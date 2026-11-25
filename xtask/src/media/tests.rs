//! Offline loader and file-ownership tests with new, small scratch files only.
//! Ownership/Lock Ordering: each test owns its own staging directory.
//! Invariants: no process launch, disk mutation outside the fixture or network access.
use super::*;
use crate::config::Disk;
use pauli_config::{KERNEL_OFFSET, loader::GUEST_DTBS_OFFSET};
use sha2::{Digest, Sha256};
fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}
#[test]
fn diskless_firmware_stages_at_the_same_addresses_the_board_will_read() {
    use crate::elf::firmware::tests::example;
    let root = workspace();
    let board = Board::load(&root, "qemu-virt-riscv").unwrap();
    for (requested, expected, cpus) in [
        (None, 32, 1),
        (None, 32, 2),
        (Some(64), 64, 1),
        (Some(256), 256, 1),
    ] {
        let mut scratch = Scratch::new(&root).unwrap();
        let file = scratch.bytes("control.elf", &example()).unwrap();
        let mut vms = crate::firmware::pair_files(&file, &file, cpus).unwrap();
        vms[1].config.control_events = true;
        let media = prepare(
            &mut scratch,
            &board,
            &mut vms,
            Defaults {
                host_mib: requested,
                kernel: None,
                disk: None,
                probe: None,
                initrd: None,
                managed: &[],
            },
            cpus,
        )
        .unwrap();
        assert_eq!(media.ram_mib, expected);
        assert!(media.disks.is_empty() && !media.uplink);
        let (dtb, manifest) = metadata_offsets(expected * MIB);
        assert_eq!(media.loaders.len(), 3); // one shared immutable template
        assert_eq!(media.loaders[1].address, board.ram_base + dtb);
        assert_eq!(media.loaders[2].address, board.ram_base + manifest);
        let buf = fs::read(&media.loaders[2].path).unwrap();
        let cfg = BootConfig::decode(buf.as_slice().try_into().unwrap()).unwrap();
        let [Some(a), Some(b), ..] = cfg.vms else {
            panic!("missing pair")
        };
        assert_eq!(a.firmware, b.firmware);
        assert_eq!(a.image.initrd_size, 0);
        assert_ne!(a.host_base, b.host_base);
        let n = usize::from(cpus);
        for boot in 0..n {
            assert_eq!(a.owner(0, 0, boot, n), boot);
            assert_eq!(b.owner(1, 0, boot, n), (boot + 1) % n);
        }
        assert_eq!(
            b.cpu_affinity == pauli_config::CpuAffinity::Workers,
            cpus > 1
        );
        let bytes = fs::read(&media.loaders[1].path).unwrap();
        let dtb = pauli_hal::fdt::Fdt::new(&bytes[..a.dtb_size as usize]).unwrap();
        assert!(dtb.find("/pauli-control").unwrap().is_none());
        let start = b.dtb_offset as usize;
        let peer = &bytes[start..start + b.dtb_size as usize];
        assert!(b.control_events && !b.host_stats);
        pauli_config::guest_dtb::validate_control_events(peer, &b, GuestBoardLayout::DEFAULT)
            .unwrap();
        assert_eq!(
            dtb.find("/").unwrap().unwrap().text("compatible").unwrap(),
            Some("openion,firmware-guest")
        );
        let cpu = dtb.find("/cpus/cpu@0").unwrap().unwrap();
        assert_eq!(
            cpu.text("riscv,isa").unwrap(),
            Some("rv64imac_zicsr_zifencei_zicntr")
        );
    }
}
fn fixture(scratch: &mut Scratch) -> Input {
    let mut image = [0; 64];
    image[8..16].copy_from_slice(&KERNEL_OFFSET.to_le_bytes());
    image[16..24].copy_from_slice(&(2 * MIB).to_le_bytes());
    image[56..60].copy_from_slice(b"RSC\x05");
    Input::local(&scratch.bytes("linux.fake", &image).unwrap())
}

#[test]
fn exclusive_toml_reaches_the_manifest_and_leaves_a_management_hart() {
    let root = workspace();
    let board = Board::load(&root, "qemu-virt-riscv").unwrap();
    let mut scratch = Scratch::new(&root).unwrap();
    let linux = fixture(&mut scratch);
    let run = |scratch: &mut Scratch, tag: &str, host_cpus, fields: &str| -> Result<BootConfig> {
        let profile = format!("[[vm]]\nname='rt'\ncpu_affinity=[0]\n{fields}");
        let path = scratch.bytes(&format!("{tag}.toml"), profile.as_bytes())?;
        let (mut vms, managed) = crate::config::profile(&path)?;
        let media = prepare(
            scratch,
            &board,
            &mut vms,
            Defaults {
                host_mib: None,
                kernel: Some(&linux),
                initrd: None,
                disk: None,
                managed: &managed,
                probe: None,
            },
            host_cpus,
        )?;
        let bytes = fs::read(&media.loaders.last().unwrap().path)?;
        Ok(BootConfig::decode(bytes.as_slice().try_into().unwrap()).unwrap())
    };
    let cfg = run(
        &mut scratch,
        "valid",
        2,
        "exclusive_cpus=true\n[[vm]]\nname='auto'",
    )
    .unwrap();
    assert!(cfg.vms[0].unwrap().exclusive_cpus);
    assert!(!cfg.vms[1].unwrap().exclusive_cpus);
    assert_eq!(cfg.exclusive_owner_mask(1, 2), Ok(1));
    assert_eq!(
        cfg.exclusive_owner_mask(0, 2),
        Err(pauli_config::Error::Affinity)
    );

    for (fields, hosts) in [
        ("exclusive_cpus=true", 1),
        (
            "exclusive_cpus=true\n[[vm]]\nname='workers'\ncpu_affinity='workers'",
            2,
        ),
        (
            "exclusive_cpus=true\n[[vm]]\nname='overlap'\ncpu_affinity=[0]",
            2,
        ),
        ("exclusive_cpus='true'", 2),
    ] {
        let mut other = Scratch::new(&root).unwrap();
        assert!(run(&mut other, "invalid", hosts, fields).is_err());
    }
    let mut other = Scratch::new(&root).unwrap();
    assert!(
        !run(&mut other, "shared", 1, "exclusive_cpus=false")
            .unwrap()
            .vms[0]
            .unwrap()
            .exclusive_cpus
    );
}

#[test]
fn firmware_toml_pins_the_original_elf_and_does_not_inherit_linux_media() {
    use crate::elf::firmware::tests::{BASE, example};
    let root = workspace();
    let mut scratch = Scratch::new(&root).unwrap();
    let board = Board::load(&root, "qemu-virt-riscv").unwrap();
    let elf = example();
    scratch.bytes("control.elf", &elf).unwrap();
    let linux = fixture(&mut scratch);
    let initrd = Input::local(&scratch.bytes("linux.cpio", b"only Linux").unwrap());
    let profile = |hash: &str| {
        format!(
            "[[vm]]\nname='firmware'\nfirmware='control.elf'\nfirmware_sha256='{hash}'\nguest_base={BASE}\nmemory_kib=32\n[[vm]]\nname='linux'\n"
        )
    };
    for (tag, digest, valid) in [
        ("bad", "0".repeat(64), false),
        ("good", format!("{:x}", Sha256::digest(&elf)), true),
    ] {
        let path = scratch
            .bytes(&format!("{tag}.toml"), profile(&digest).as_bytes())
            .unwrap();
        let (mut vms, managed) = crate::config::profile(&path).unwrap();
        let media = prepare(
            &mut scratch,
            &board,
            &mut vms,
            Defaults {
                host_mib: None,
                kernel: Some(&linux),
                initrd: Some(&initrd),
                disk: None,
                managed: &managed,
                probe: None,
            },
            1,
        );
        if !valid {
            assert!(format!("{:#}", media.err().unwrap()).contains("SHA-256"));
            continue;
        }
        assert!(media.unwrap().ram_mib >= 256);
        assert_eq!(vms[0].config.image.initrd_size, 0);
        assert_eq!(vms[1].config.image.initrd_size, 10);
        assert_eq!(vms[1].config.host_base % ALIGN, 0);
        assert_ne!(vms[0].config.host_base, vms[1].config.host_base);
    }
}
#[test]
fn staged_layout_has_disjoint_sources_and_roundtrips() {
    let root = workspace();
    let mut scratch = Scratch::new(&root).unwrap();
    let scratch_dir = scratch.directory.clone();
    let board = Board::load(&root, "qemu-virt-riscv").unwrap();
    let kernel = fixture(&mut scratch);
    let mut vms = [
        Vm::probe("small", b'I').unwrap(),
        Vm::probe("large", b'I').unwrap(),
    ];
    vms[1].config.memory_size = 256 * MIB;
    let media = prepare(
        &mut scratch,
        &board,
        &mut vms,
        Defaults {
            host_mib: None,
            kernel: Some(&kernel),
            initrd: None,
            disk: None,
            managed: &[],
            probe: Some(b"fixture"),
        },
        2,
    )
    .unwrap();
    assert_eq!(media.loaders.len(), 4); // shared Image + shared initrd + DTBs + manifest
    let manifest = fs::read(scratch_dir.join("boot.bin")).unwrap();
    let config = BootConfig::decode(manifest.as_slice().try_into().unwrap()).unwrap();
    let [Some(a), Some(b), ..] = config.vms else {
        panic!("missing VMs")
    };
    assert!(a.host_base + a.memory_size <= board.ram_base + GUEST_DTBS_OFFSET);
    assert!(b.host_base >= config.template_base + config.template_size);
    assert_eq!(a.image, b.image);
    assert_ne!(a.dtb_offset, b.dtb_offset);
    assert!(board.ram_base + media.ram_mib * MIB >= b.host_base + b.memory_size + NATIVE_TAIL);
    drop(scratch);
    assert!(!scratch_dir.exists());
}
#[test]
fn invalid_image_and_manifest_do_not_become_launchable() {
    let root = workspace();
    let mut scratch = Scratch::new(&root).unwrap();
    let board = Board::load(&root, "qemu-virt-riscv").unwrap();
    let mut kernel = fixture(&mut scratch);
    kernel.sha256 = Some("0".repeat(64));
    let mut vm = [Vm::probe("wrong", b'L').unwrap()];
    let make = |scratch: &mut Scratch, vms: &mut [Vm], source: &Input| {
        prepare(
            scratch,
            &board,
            vms,
            Defaults {
                host_mib: None,
                kernel: Some(source),
                initrd: None,
                disk: None,
                managed: &[],
                probe: Some(b"fixture"),
            },
            1,
        )
    };
    assert!(make(&mut scratch, &mut vm, &kernel).is_err());
    kernel.sha256 = None;
    vm[0].config.memory_size = 8; // must fail before computing the DTB initrd offset
    assert!(make(&mut scratch, &mut vm, &kernel).is_err());
}
#[test]
fn disk_path_alias_never_creates_an_independent_writer() {
    let root = workspace();
    let mut scratch = Scratch::new(&root).unwrap();
    let (path, file) = scratch.file("disk.raw").unwrap();
    file.set_len(4096).unwrap();
    drop(file);
    let alias = scratch.directory.join(".").join("disk.raw");
    let [a, b] = [Input::local(&path), Input::local(&alias)];
    let mut first = Vm::probe("first", b'D').unwrap();
    let mut second = Vm::probe("second", b'E').unwrap();
    first.disk_mode = Disk::Writable;
    second.disk_mode = Disk::ReadOnly;
    let mut disks = Vec::new();
    bind(&mut first, Some(&a), &mut disks).unwrap();
    assert!(bind(&mut second, Some(&b), &mut disks).is_err());
    assert_eq!(disks.len(), 1);
    disks.clear();
    first.disk_mode = Disk::ReadOnly;
    bind(&mut first, Some(&a), &mut disks).unwrap();
    bind(&mut second, Some(&b), &mut disks).unwrap();
    assert_eq!(first.config.disk, second.config.disk);
}
#[test]
fn managed_media_roundtrips_and_aliases_never_become_independent_volumes() {
    let root = workspace();
    let mut scratch = Scratch::new(&root).unwrap();
    let board = Board::load(&root, "qemu-virt-riscv").unwrap();
    let kernel = fixture(&mut scratch);
    let sector = [0x59; 512];
    let path = scratch.bytes("volume.raw", &sector).unwrap();
    let digest = format!("{:x}", Sha256::digest(sector));
    let profile = format!(
        "[[vm]]\nname='template'\n[[managed_disk]]\nimage='volume.raw'\nsha256='{digest}'\nmode='ro'\n"
    );
    let config_path = scratch.bytes("profile.toml", profile.as_bytes()).unwrap();
    let (mut vms, managed) = crate::config::profile(&config_path).unwrap();
    let opts = Defaults {
        host_mib: None,
        kernel: Some(&kernel),
        initrd: None,
        disk: None,
        managed: &managed,
        probe: None,
    };
    let media = prepare(&mut scratch, &board, &mut vms, opts, 1).unwrap();
    assert_eq!(media.disks.len(), 1);
    let wire = fs::read(scratch.directory.join("boot.bin")).unwrap();
    let config = BootConfig::decode(wire.as_slice().try_into().unwrap()).unwrap();
    let disk = config.disks[0].unwrap();
    assert!(disk.managed && !disk.writable && disk.sectors == 1);
    assert_eq!(config.vms[0].unwrap().disk, None);
    // A second name for the same inode/file ID is not another volume.
    let alias = scratch.directory.join("hardlink.raw");
    fs::hard_link(&path, &alias).unwrap();
    scratch.files.push(alias.clone());
    let alias = Input::local(&alias);
    let mut bound = media.disks;
    for (managed, writable) in [(true, false), (true, true), (false, false), (false, true)] {
        assert!(crate::disk::register(&alias, writable, managed, &mut bound).is_err());
    }
    assert_eq!(fs::read(&path).unwrap(), sector);
}

#[test]
fn managed_disk_mode_and_digest_are_not_implicit() {
    let mut scratch = Scratch::new(&workspace()).unwrap();
    for (i, tail) in [
        "mode='rw'",
        "sha256='bad'\nmode='ro'",
        "sha256='0000000000000000000000000000000000000000000000000000000000000000'",
    ]
    .into_iter()
    .enumerate()
    {
        let profile = format!("[[vm]]\nname='vm'\n[[managed_disk]]\nimage='disk.raw'\n{tail}\n");
        let path = scratch
            .bytes(&format!("invalid-{i}.toml"), profile.as_bytes())
            .unwrap();
        assert!(crate::config::profile(&path).is_err());
    }
}
