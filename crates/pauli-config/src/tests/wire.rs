//! Canonical manifest encoding and large-initrd ABI oracles.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

use super::*;

#[test]
fn host_stats_requires_an_explicit_linux_management_boot_slot() {
    let mut cfg = fixture();
    cfg.smoke_payload = false;
    cfg.vms[0] = Some(cfg.vms[0].unwrap().renamed("manager").unwrap());
    let mut raw = cfg.encode().unwrap();
    assert!(!BootConfig::decode(&raw).unwrap().boot_stats_for(0));
    raw[crate::HEADER + 73] |= 0x10;
    cfg.vms[0].as_mut().unwrap().host_stats = true;
    assert_eq!(cfg.encode().unwrap(), raw);
    let decoded = BootConfig::decode(&raw).unwrap();
    assert!(decoded.boot_stats_for(0));
    for slot in [1, 2, 3, 4, usize::MAX] {
        assert!(!decoded.boot_stats_for(slot));
    }
    assert!(!decoded.vms[0].unwrap().renamed("clone").unwrap().host_stats);

    for mutate in [
        (|cfg: &mut BootConfig| cfg.smoke_payload = true) as fn(&mut BootConfig),
        |cfg| {
            cfg.vms[1..].fill(None);
            cfg.management = false;
        },
        |cfg| {
            cfg.vms[0].as_mut().unwrap().firmware = Some(crate::FirmwareLayout {
                rx_pages: 1,
                ro_pages: 0,
            })
        },
        |cfg| cfg.vms[1].as_mut().unwrap().host_stats = true,
    ] {
        let mut invalid = cfg;
        mutate(&mut invalid);
        assert_eq!(invalid.encode(), Err(Error::Reserved));
    }
    let mut invalid_wire = raw;
    invalid_wire[crate::HEADER + crate::RECORD + 73] |= 0x10;
    assert!(matches!(
        BootConfig::decode(&invalid_wire),
        Err(Error::Reserved)
    ));
}

#[test]
fn large_initrd_placement() {
    // Independent placement oracle: large initrds grow down from DTB, while
    // all formerly valid <=4 MiB inputs retain their original guest address.
    let mut large = fixture();
    large.vms[1..].fill(None);
    large.management = false;
    large.template_base = 0x9000_0000;
    large.template_size = 256 * 1024 * 1024;
    large.vms[0].as_mut().unwrap().host_base = 0xa000_0000;
    for ram in [64, 96, 128, 256, 512, 1024] {
        for archive in [0, 4, 30, 31, 32, 64, 128, 129] {
            let vm = large.vms[0].as_mut().unwrap();
            vm.memory_size = ram * 1024 * 1024;
            vm.image.kernel_source = 0x9000_0000;
            vm.image.kernel_size = 28 * 1024 * 1024;
            vm.image.kernel_memory_size = 30 * 1024 * 1024;
            vm.image.initrd_source = if archive == 0 { 0 } else { 0x9400_0000 };
            vm.image.initrd_size = archive * 1024 * 1024;
            let valid = archive <= 128 && archive <= ram - 4;
            let offset = if valid {
                ((ram * 1024 * 1024 - 2 * 1024 * 1024 - archive * 1024 * 1024)
                    & !((2 * 1024 * 1024) - 1))
                    .min((ram - 32) * 1024 * 1024)
            } else {
                0
            };
            assert_eq!(
                large.validate().is_ok(),
                valid && offset >= 32 * 1024 * 1024
            );
            if large.validate().is_ok() {
                assert_eq!(large.vms[0].unwrap().initrd_offset(), offset);
                let raw = large.encode().unwrap();
                assert_eq!(BootConfig::decode(&raw).unwrap().encode().unwrap(), raw);
            }
        }
    }
    let mut bad_large = large;
    bad_large.vms[0].as_mut().unwrap().memory_size = 1026 * 1024 * 1024;
    assert_eq!(bad_large.validate(), Err(Error::Memory));
}

#[test]
fn canonical_wire_mutations() {
    let config = fixture();
    let wire = config.encode().unwrap();
    assert_eq!(BootConfig::decode(&wire).unwrap().encode().unwrap(), wire);
    for aia in [false, true] {
        for disk in [None, Some(0), Some(1), Some(2), Some(3)] {
            let mut selected = config;
            if let Some(first) = disk {
                selected.disks = [Some(DiskConfig {
                    sectors: 512,
                    writable: false,
                    managed: false,
                }); 4];
                // Every declared backend has exactly one owner. Permute the
                // four bindings while exercising the new flag beside disk bits.
                for (index, vm) in selected.vms.iter_mut().flatten().enumerate() {
                    vm.disk = Some((first + index as u8) % 4);
                }
            }
            selected.vms[0].as_mut().unwrap().aia = aia;
            selected.vms[0].as_mut().unwrap().disk = disk;
            let wire = selected.encode().unwrap();
            let decoded = BootConfig::decode(&wire).unwrap();
            assert_eq!(decoded.vms[0].unwrap().aia, aia);
            assert_eq!(decoded.vms[0].unwrap().disk, disk);
            assert_eq!(decoded.encode().unwrap(), wire);
        }
    }
    let mut stale = wire;
    stale[..8].copy_from_slice(b"IONGST10");
    assert!(matches!(BootConfig::decode(&stale), Err(Error::Version)));
    stale[..8].copy_from_slice(b"IONGST13");
    assert!(matches!(BootConfig::decode(&stale), Err(Error::Version)));
    // Every accepted single-bit change must still have a canonical encoding;
    // malformed UTF-8, padding, indices and overflowing ranges must not panic.
    for i in 0..WIRE_SIZE {
        for bit in 0..8 {
            let mut mutated = wire;
            mutated[i] ^= 1 << bit;
            if let Ok(decoded) = BootConfig::decode(&mutated) {
                assert_eq!(decoded.encode().unwrap(), mutated);
            }
        }
    }
}
