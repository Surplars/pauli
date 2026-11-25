//! Manifest compatibility and placement on logical native owners, including owner 63.
use super::*;
use crate::{HEADER, MAX_GUEST_VCPUS};

fn pin(vm: &mut VmConfig, exclusive: bool, owners: &[u8]) {
    let mut pins = [0; MAX_GUEST_VCPUS as usize];
    pins[..owners.len()].copy_from_slice(owners);
    vm.vcpus = owners.len() as u8;
    vm.cpu_affinity = CpuAffinity::Physical(pins);
    vm.exclusive_cpus = exclusive;
}

#[test]
fn exclusive_flag_occupies_only_bit_eleven_in_v15() {
    let mut cfg = fixture();
    cfg.vms[1..].fill(None);
    let vm = cfg.vms[0].as_mut().unwrap();
    assert!(!vm.exclusive_cpus);
    vm.autostart = true;
    vm.aia = true;
    pin(vm, false, &[63]);

    // Original IONGST15 fixture, including its padding and legacy pin encoding.
    let mut legacy = [0u8; WIRE_SIZE];
    legacy[..8].copy_from_slice(b"IONGST15");
    for (i, word) in [0x8e00_0000u64, 30 << 20, 4096, 1, 0, 1, 0]
        .into_iter()
        .enumerate()
    {
        let pos = 8 + i * 8;
        legacy[pos..pos + 8].copy_from_slice(&word.to_le_bytes());
    }
    legacy[HEADER..HEADER + 6].copy_from_slice(b"guest0");
    for (i, word) in [
        0x8000_0000u64,
        0x9000_0000,
        64 << 20,
        0,
        1024,
        100,
        0x401,
        1,
        (1 << 32) | 63,
        0x8e00_0000,
        64,
        4096,
        0,
        0,
    ]
    .into_iter()
    .enumerate()
    {
        let pos = HEADER + 24 + i * 8;
        legacy[pos..pos + 8].copy_from_slice(&word.to_le_bytes());
    }
    assert_eq!(cfg.encode().unwrap(), legacy);
    assert!(
        !BootConfig::decode(&legacy).unwrap().vms[0]
            .unwrap()
            .exclusive_cpus
    );

    cfg.vms[0].as_mut().unwrap().exclusive_cpus = true;
    let mut extended = legacy;
    extended[HEADER + 73] |= 0x08;
    assert_eq!(cfg.encode().unwrap(), extended);
    let decoded = BootConfig::decode(&extended).unwrap();
    assert!(decoded.vms[0].unwrap().exclusive_cpus);
    assert_eq!(decoded.encode().unwrap(), extended);
    extended[HEADER + 73] |= 0x10;
    assert!(matches!(
        BootConfig::decode(&extended),
        Err(Error::Reserved)
    ));
}

#[test]
fn manifest_rejects_invalid_exclusive_pins_in_either_record_order() {
    for affinity in [CpuAffinity::Automatic, CpuAffinity::Workers] {
        let mut cfg = fixture();
        let vm = cfg.vms[0].as_mut().unwrap();
        vm.exclusive_cpus = true;
        vm.cpu_affinity = affinity;
        assert_eq!(vm.validate_placement(64), Err(Error::Affinity));
        assert_eq!(cfg.validate(), Err(Error::Affinity));
    }
    for owners in [&[2, 2][..], &[64], &[u8::MAX]] {
        let mut cfg = fixture();
        pin(cfg.vms[0].as_mut().unwrap(), true, owners);
        assert_eq!(cfg.validate(), Err(Error::Affinity));
        assert_eq!(cfg.exclusive_owner_mask(0, 64), Err(Error::Affinity));
    }
    for (left, right) in [(true, true), (true, false), (false, true), (false, false)] {
        let mut cfg = fixture();
        pin(cfg.vms[0].as_mut().unwrap(), left, &[0, 63]);
        pin(cfg.vms[1].as_mut().unwrap(), right, &[63, 2]);
        let result = if left || right {
            Err(Error::Affinity)
        } else {
            Ok(())
        };
        assert_eq!(cfg.validate(), result);
        assert_eq!(cfg.exclusive_owner_mask(17, 64).map(|_| ()), result);
    }
}

#[test]
fn boot_and_worker_constraints_use_the_actual_topology() {
    let mut cfg = fixture();
    pin(cfg.vms[0].as_mut().unwrap(), true, &[0, 2]);
    pin(cfg.vms[1].as_mut().unwrap(), true, &[3]);
    // Native owner zero can be a dedicated worker when the boot hart is one.
    assert_eq!(cfg.validate(), Ok(()));
    assert_eq!(cfg.exclusive_owner_mask(1, 4), Ok(0b1101));
    for (boot, host) in [
        (0, 4),
        (2, 4),
        (3, 4),
        (1, 3),
        (4, 4),
        (0, 0),
        (1, 65),
        (0, usize::MAX),
    ] {
        assert_eq!(cfg.exclusive_owner_mask(boot, host), Err(Error::Affinity));
    }
    cfg.vms[2].as_mut().unwrap().cpu_affinity = CpuAffinity::Workers;
    assert_eq!(cfg.validate(), Ok(())); // The manifest has no actual host count.
    assert_eq!(cfg.exclusive_owner_mask(1, 4), Err(Error::Affinity));
    assert_eq!(cfg.exclusive_owner_mask(1, 5), Ok(0b1101));
    let workers = cfg.vms[2].unwrap();
    assert_eq!(
        workers.validate_placement_excluding(1, 4, 0b1101),
        Err(Error::Affinity)
    );
    assert_eq!(workers.validate_placement_excluding(1, 5, 0b1101), Ok(()));
    let mut fixed = cfg.vms[0].unwrap();
    assert_eq!(fixed.validate_placement_excluding(1, 4, 0b1101), Ok(()));
    assert_eq!(
        fixed.validate_placement_excluding(1, 4, 0b1001),
        Err(Error::Affinity)
    );
    fixed.exclusive_cpus = false;
    assert_eq!(
        fixed.validate_placement_excluding(1, 4, 0b1101),
        Err(Error::Affinity)
    );
    for invalid in [1 << 1, 1 << 4, 1 << 63] {
        assert_eq!(
            fixed.validate_placement_excluding(1, 4, invalid),
            Err(Error::Affinity)
        );
    }
    pin(cfg.vms[0].as_mut().unwrap(), true, &[63]);
    assert_eq!(cfg.exclusive_owner_mask(1, 64), Ok(1u64 << 63 | 1 << 3));
}

#[test]
fn selection_counts_only_in_range_candidates() {
    let mut vm = VmConfig::new("guest").unwrap();
    vm.vcpus = 4;
    for affinity in [CpuAffinity::Automatic, CpuAffinity::Workers] {
        vm.cpu_affinity = affinity;
        // Bits outside the three-owner topology and the boot hart are not
        // candidates in this walk. Admission still rejects this reservation.
        let excluded = (1u64 << 63) | (1 << 1);
        assert_eq!(
            vm.validate_placement_excluding(1, 3, excluded),
            Err(Error::Affinity)
        );
        let owners = core::array::from_fn(|index| vm.owner_excluding(0, index, 1, 3, excluded));
        assert_eq!(owners, [2, 0, 2, 0]);
        assert_eq!(vm.owner_mask_excluding(0, 1, 3, excluded), 0b101);
    }
}

#[test]
fn distributed_placements_cycle_only_through_the_remaining_owners() {
    let mut vm = VmConfig::new("guest").unwrap();
    vm.vcpus = 8;
    for host in 1..=6 {
        for boot in 0..host {
            for reserved in (0..1u64 << host).filter(|mask| mask & (1 << boot) == 0) {
                for affinity in [CpuAffinity::Automatic, CpuAffinity::Workers] {
                    vm.cpu_affinity = affinity;
                    let ordered: Vec<_> = (0..host)
                        .map(|i| (boot + i) % host)
                        .filter(|&o| {
                            reserved & (1 << o) == 0
                                && (affinity != CpuAffinity::Workers || o != boot)
                        })
                        .collect();
                    assert_eq!(
                        vm.validate_placement_excluding(boot, host, reserved)
                            .is_ok(),
                        !ordered.is_empty()
                    );
                    if ordered.is_empty() {
                        continue;
                    }
                    for slot in [0, 1, host + 3, usize::MAX] {
                        let mut mask = 0;
                        for i in 0..usize::from(vm.vcpus) {
                            let owner = ordered[(slot % ordered.len() + i) % ordered.len()];
                            assert_eq!(vm.owner_excluding(slot, i, boot, host, reserved), owner);
                            mask |= 1usize << owner;
                        }
                        assert_eq!(vm.owner_mask_excluding(slot, boot, host, reserved), mask);
                    }
                }
            }
        }
    }
}
