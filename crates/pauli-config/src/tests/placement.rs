//! Physical/worker owner oracles over every supported topology.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

use super::*;
use crate::{HEADER, MAX_GUEST_VCPUS};

fn legacy_pins(owners: [u8; 4]) -> CpuAffinity {
    let mut pins = [0; MAX_GUEST_VCPUS as usize];
    pins[..4].copy_from_slice(&owners);
    CpuAffinity::Physical(pins)
}

#[test]
fn pinned_roundtrip() {
    let config = fixture();
    let wire = uplink_wire();
    // Explicit all-hart-0 is different from automatic placement, even though
    // both can be valid on a one-hart host. Duplicates deliberately overcommit.
    for owners in [[0, 0, 0, 0], [3, 2, 1, 0], [1, 1, 0, 0]] {
        let mut pinned = config;
        let vm = pinned.vms[0].as_mut().unwrap();
        vm.vcpus = 4;
        vm.cpu_affinity = legacy_pins(owners);
        assert!(vm.validate_placement(4).is_ok());
        for (index, owner) in owners.into_iter().enumerate() {
            assert_eq!(vm.owner(0, index, 2, 4), usize::from(owner));
        }
        let raw = pinned.encode().unwrap();
        assert_eq!(
            u64::from_le_bytes(raw[152..160].try_into().unwrap()),
            (1 << 32) | u64::from(u32::from_le_bytes(owners))
        );
        assert_ne!(raw, wire);
        assert_eq!(BootConfig::decode(&raw).unwrap().encode().unwrap(), raw);
        // Includes every affinity byte, presence bit, unused and reserved bits.
        for at in 64 + 88..64 + 96 {
            for bit in 0..8 {
                let mut mutated = raw;
                mutated[at] ^= 1 << bit;
                if let Ok(decoded) = BootConfig::decode(&mutated) {
                    assert_eq!(decoded.encode().unwrap(), mutated);
                }
            }
        }
    }
}

#[test]
fn worker_roundtrip() {
    let config = fixture();
    // Worker placement excludes the actual boot hart, including boot hart 1.
    // Exhaust every supported topology, VM slot and overcommitted vCPU count.
    let mut workers = config;
    workers.vms[0].as_mut().unwrap().cpu_affinity = CpuAffinity::Workers;
    assert_eq!(
        workers.vms[0].unwrap().validate_placement(1),
        Err(Error::Affinity)
    );
    for (host, boot) in [2usize, 3, 4, 5, 8, 32, 64]
        .into_iter()
        .flat_map(|host| (0..host).map(move |boot| (host, boot)))
    {
        for slot in 0..4 {
            for count in 1..=MAX_GUEST_VCPUS {
                let vm = workers.vms[0].as_mut().unwrap();
                vm.vcpus = count;
                assert!(vm.validate_placement(host as u8).is_ok());
                let mut counts = [0usize; crate::MAX_HOST_CPUS as usize];
                for index in 0..usize::from(count) {
                    let owner = vm.owner(slot, index, boot, host);
                    assert!(owner < host && owner != boot);
                    counts[owner] += 1;
                }
                let active: Vec<_> = (0..host)
                    .filter(|hart| *hart != boot)
                    .map(|hart| counts[hart])
                    .collect();
                assert!(active.iter().max().unwrap() - active.iter().min().unwrap() <= 1);
                let raw = workers.encode().unwrap();
                assert_eq!(
                    u64::from_le_bytes(raw[152..160].try_into().unwrap()),
                    2 << 32
                );
                assert_eq!(BootConfig::decode(&raw).unwrap().encode().unwrap(), raw);
            }
        }
    }
    let raw = workers.encode().unwrap();
    for at in 152..160 {
        for bit in 0..8 {
            let mut mutated = raw;
            mutated[at] ^= 1 << bit;
            if let Ok(decoded) = BootConfig::decode(&mutated) {
                assert_eq!(decoded.encode().unwrap(), mutated);
            }
        }
    }
    let mut old = raw;
    old[..8].copy_from_slice(b"IONGST14");
    assert!(BootConfig::decode(&old).is_err());
}

#[test]
fn exhaustive_placement() {
    let config = fixture();
    // Exhaust the four-hart placement space for every host/vCPU count. The
    // oracle reads the requested array directly, independently of owner().
    for (count, host) in (1..=4).flat_map(|count| (1..=4).map(move |host| (count, host))) {
        for packed in 0..256u16 {
            let owners: [u8; 4] = core::array::from_fn(|i| ((packed >> (2 * i)) & 3) as u8);
            let mut pinned = config;
            let vm = pinned.vms[0].as_mut().unwrap();
            vm.vcpus = count;
            vm.cpu_affinity = legacy_pins(owners);
            let valid = owners[..usize::from(count)].iter().all(|hart| *hart < host)
                && owners[usize::from(count)..].iter().all(|hart| *hart == 0);
            assert_eq!(vm.validate_placement(host).is_ok(), valid);
            if valid {
                for (index, hart) in owners[..usize::from(count)].iter().enumerate() {
                    assert_eq!(vm.owner(0, index, 0, usize::from(host)), usize::from(*hart));
                }
                let raw = pinned.encode().unwrap();
                assert_eq!(BootConfig::decode(&raw).unwrap().encode().unwrap(), raw);
            }
        }
    }
}

#[test]
fn placement_and_overflow_errors() {
    let config = fixture();
    let mut bad;
    let vm = config.vms[0].as_ref().unwrap();
    assert_eq!(vm.owner(1, 0, 2, 4), 3);
    assert_eq!(vm.owner(2, 0, 2, 4), 0);
    bad = config;
    let vm = bad.vms[0].as_mut().unwrap();
    vm.cpu_affinity = legacy_pins([1, 0, 0, 0]);
    assert!(vm.validate_placement(2).is_ok());
    assert_eq!(vm.validate_placement(1), Err(Error::Affinity));
    vm.cpu_affinity = legacy_pins([crate::MAX_HOST_CPUS, 0, 0, 0]);
    assert_eq!(bad.validate(), Err(Error::Affinity));
    bad = config;
    bad.vms[0].as_mut().unwrap().cpu_affinity = legacy_pins([0, 1, 0, 0]);
    assert_eq!(bad.validate(), Err(Error::Affinity)); // Inactive vCPU padding.
    bad = config;
    bad.vms[0].as_mut().unwrap().dtb_offset = u64::MAX;
    assert_eq!(bad.validate(), Err(Error::Dtb));
    bad = config;
    bad.vms[0].as_mut().unwrap().guest_base = u64::MAX - 0x1f_ffff;
    assert_eq!(bad.validate(), Err(Error::Memory));
}

#[test]
fn wide_pins_are_canonical_without_reinterpreting_legacy_bytes() {
    for n in 5..=MAX_GUEST_VCPUS {
        let mut config = fixture();
        let vm = config.vms[0].as_mut().unwrap();
        vm.vcpus = n;
        let pins = core::array::from_fn(|i| if i < usize::from(n) { (i % 4) as u8 } else { 0 });
        vm.cpu_affinity = CpuAffinity::Physical(pins);
        let raw = config.encode().unwrap();
        let restored = BootConfig::decode(&raw).unwrap();
        assert_eq!(
            restored.vms[0].unwrap().cpu_affinity,
            CpuAffinity::Physical(pins)
        );
        assert_eq!(restored.encode().unwrap(), raw);
        let at = HEADER + 88;
        let mut bad = raw;
        bad[at + 4] = 1; // A wide VM cannot use the tag with only four owner bytes.
        assert!(BootConfig::decode(&bad).is_err());
        bad = raw;
        bad[HEADER + 80] = 4; // Nor may a narrow VM use the wide encoding.
        assert!(BootConfig::decode(&bad).is_err());
        if n < MAX_GUEST_VCPUS {
            let bit = usize::from(n) * 2;
            bad = raw;
            bad[at + bit / 8] |= 1 << (bit % 8);
            assert!(matches!(BootConfig::decode(&bad), Err(Error::Affinity)));
        }
    }
}

#[test]
fn high_native_owners_roundtrip_without_retargeting_or_alias_encodings() {
    let at = HEADER + 80;
    for count in 1..=MAX_GUEST_VCPUS {
        for pos in 0..usize::from(count) {
            let mut cfg = fixture();
            let vm = cfg.vms[0].as_mut().unwrap();
            vm.vcpus = count;
            let mut pins = [0; MAX_GUEST_VCPUS as usize];
            pins[pos] = 63;
            vm.cpu_affinity = CpuAffinity::Physical(pins);
            assert_eq!(vm.validate_placement(63), Err(Error::Affinity));
            assert_eq!(vm.validate_placement(64), Ok(()));
            let bytes = cfg.encode().unwrap();
            let decoded = BootConfig::decode(&bytes).unwrap();
            assert_eq!(
                decoded.vms[0].unwrap().cpu_affinity,
                CpuAffinity::Physical(pins)
            );
            assert_eq!(decoded.encode().unwrap(), bytes);
            if count > 4 {
                assert_eq!(bytes[at + 15], 4);
                for offset in [7, 14] {
                    // reserved top bytes of the two 48-bit halves
                    let mut bad = bytes;
                    bad[at + offset] = 1;
                    assert!(BootConfig::decode(&bad).is_err());
                }
                let mut short = bytes;
                short[at] = 4;
                assert!(BootConfig::decode(&short).is_err());
            }
        }
    }
    let mut cfg = fixture();
    cfg.vms[0].as_mut().unwrap().vcpus = 16;
    let mut noncanonical = cfg.encode().unwrap();
    noncanonical[at + 15] = 4; // all owners zero already have a compact encoding
    assert!(BootConfig::decode(&noncanonical).is_err());
    let mut bad = noncanonical;
    bad[at] = 8;
    bad[at + 1] = 63; // ninth explicit owner on an eight-vCPU guest
    assert!(BootConfig::decode(&bad).is_err());
}
