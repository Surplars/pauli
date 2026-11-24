//! Build the launch command without spawning an emulator or opening guest files.
use super::*;

#[test]
fn wave8_uses_the_flat_package_payload_slot_and_cannot_launch_as_qemu_virt() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let board = Board::load(&root, "valence-wave8-ooo")?;
    // FW_DYNAMIC's next address through the first byte of the separate initrd slot.
    assert_eq!(
        (board.entry, board.entry + board.limit),
        (0x8020_0000, 0x8100_0000)
    );
    assert_eq!((board.max_cpus, board.hz), (1, 100_000_000));
    assert!(Platform::new(&board, &Options::default(), &[], Mode::Normal).is_err());
    Ok(())
}

#[test]
fn observing_does_not_select_the_selftest_payload_or_change_firmware_isa() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let board = Board::load(&root, "qemu-virt-riscv")?;
    let elf = Path::new("fixture.elf");
    let vms = crate::firmware::pair_files(elf, elf, 2)?;
    for (mode, counters, boot_test) in [
        (Mode::Normal, false, false),
        (Mode::Observed, true, false),
        (Mode::HvSelftest, true, true),
    ] {
        let p = Platform::new(&board, &Options::default(), &vms, mode)?;
        assert_eq!(p.features.split(',').any(|f| f == "selftest"), counters);
        let cmd = p.command(elf, 2, None, false)?;
        let args: Vec<_> = cmd.get_args().map(|arg| arg.to_str().unwrap()).collect();
        let cpu = args.windows(2).find(|w| w[0] == "-cpu").unwrap()[1];
        assert!(cpu.ends_with(&format!("f={boot_test},d={boot_test}")));
        assert_eq!(args.contains(&"-append"), boot_test);
        assert_eq!(args.contains(&"pauli.selftest"), boot_test);
    }
    Ok(())
}

#[test]
fn native_ipi_and_firmware_latency_share_the_same_emulated_hardware() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let board = Board::load(&root, "qemu-virt-riscv")?;
    let elf = Path::new("fixture.elf"); // Ignore which ELF boots; compare the machine.
    let vms = crate::firmware::pair_files(elf, elf, 2)?;
    let options = Options::default();
    let native = Platform::new(&board, &options, &[], Mode::Normal)?.machine(
        elf,
        2,
        media::FIRMWARE_RAM_MIB,
    );
    let media = Media {
        loaders: vec![media::Loader {
            path: "firmware.bin".into(),
            address: 0x1000,
        }],
        disks: Vec::new(),
        uplink: false,
        ram_mib: media::FIRMWARE_RAM_MIB,
    };
    let guest = Platform::new(&board, &options, &vms, Mode::Observed)?.command(
        elf,
        2,
        Some(&media),
        false,
    )?;
    let direct: Vec<_> = native.get_args().map(|s| s.to_str().unwrap()).collect();
    let vm: Vec<_> = guest.get_args().map(|s| s.to_str().unwrap()).collect();
    assert_eq!(
        vm.strip_prefix(direct.as_slice()),
        Some(
            [
                "-device",
                "loader,file=firmware.bin,addr=0x1000,force-raw=on"
            ]
            .as_slice()
        )
    );
    for pair in [
        ["-machine", "virt"],
        ["-smp", "2"],
        ["-m", "32M"],
        ["-nic", "none"],
    ] {
        assert!(direct.windows(2).any(|w| w == pair));
    }
    assert!(
        !direct
            .iter()
            .any(|v| matches!(*v, "-device" | "-drive" | "-netdev" | "-initrd"))
    );
    Ok(())
}
