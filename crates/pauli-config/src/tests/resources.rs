//! Private RAM, disk writer exclusivity and reciprocal network ownership.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

use super::*;

#[test]
fn memory_and_shared_disks() {
    let config = fixture();
    let mut bad = config;
    bad.vms[1].as_mut().unwrap().host_base = bad.vms[0].unwrap().host_base;
    assert_eq!(bad.validate(), Err(Error::Memory));
    bad = config;
    bad.vms[1] = None;
    assert_eq!(bad.validate(), Err(Error::Count));
    bad = config;
    bad.disks[0] = Some(DiskConfig {
        sectors: 32,
        writable: false,
        managed: false,
    });
    for vm in bad.vms.iter_mut().flatten() {
        vm.disk = Some(0);
    }
    assert!(bad.validate().is_ok()); // Shared read-only backing is intentional.
    bad.vms[0].as_mut().unwrap().writable = true;
    assert_eq!(bad.validate(), Err(Error::Disk));
    bad.disks[0].as_mut().unwrap().writable = true;
    assert_eq!(bad.validate(), Err(Error::Disk)); // A writer excludes readers too.
    for vm in bad.vms.iter_mut().skip(1).flatten() {
        vm.disk = None;
    }
    assert!(bad.validate().is_ok()); // One writer; peer disk interfaces absent.
}

#[test]
fn network_bindings() {
    let independent = independent();
    let mut paired = independent;
    let mut uplink = independent;
    uplink.vms[0].as_mut().unwrap().net_uplink = true;
    uplink.vms[0].as_mut().unwrap().stop_on_reset = true;
    let wire = uplink.encode().unwrap();
    let decoded = BootConfig::decode(&wire).unwrap();
    assert!(decoded.vms[0].unwrap().net_uplink && decoded.vms[0].unwrap().stop_on_reset);
    assert_eq!(decoded.encode().unwrap(), wire);
    let mut old = wire;
    old[..8].copy_from_slice(b"IONGST12");
    assert_eq!(BootConfig::decode(&old).err(), Some(Error::Version));
    uplink.vms[1].as_mut().unwrap().net_uplink = true;
    assert_eq!(uplink.validate(), Err(Error::Network));
    paired.vms[0].as_mut().unwrap().net_peer = Some(1);
    paired.vms[1].as_mut().unwrap().net_peer = Some(0);
    paired.vms[2].as_mut().unwrap().net_peer = Some(3);
    paired.vms[3].as_mut().unwrap().net_peer = Some(2);
    let mut collision = paired;
    collision.vms[0].as_mut().unwrap().net_uplink = true;
    assert_eq!(collision.validate(), Err(Error::Network));
    let network = paired.encode().unwrap();
    assert_eq!(
        BootConfig::decode(&network).unwrap().encode().unwrap(),
        network
    );
    let mut invalid = paired;
    invalid.vms[0].as_mut().unwrap().net_peer = Some(0);
    assert_eq!(invalid.validate(), Err(Error::Network));
    invalid = paired;
    invalid.vms[1].as_mut().unwrap().net_peer = None;
    assert_eq!(invalid.validate(), Err(Error::Network));
    invalid = paired;
    invalid.vms[0].as_mut().unwrap().net_peer = Some(4);
    assert_eq!(invalid.validate(), Err(Error::Network));
    invalid = paired;
    invalid.vms[0].as_mut().unwrap().net_peer = Some(3);
    assert_eq!(invalid.validate(), Err(Error::Network));
    for at in 0..WIRE_SIZE {
        for bit in 0..8 {
            let mut mutated = network;
            mutated[at] ^= 1 << bit;
            if let Ok(decoded) = BootConfig::decode(&mutated) {
                assert_eq!(decoded.encode().unwrap(), mutated);
            }
        }
    }
}

#[test]
fn independent_disk_bindings() {
    let independent = independent();
    let mut bad;
    let raw = independent.encode().unwrap();
    assert_eq!(BootConfig::decode(&raw).unwrap().encode().unwrap(), raw);
    for i in 0..WIRE_SIZE {
        for bit in 0..8 {
            let mut mutated = raw;
            mutated[i] ^= 1 << bit;
            if let Ok(decoded) = BootConfig::decode(&mutated) {
                assert_eq!(decoded.encode().unwrap(), mutated);
            }
        }
    }
    bad = independent;
    bad.vms[0].as_mut().unwrap().disk = Some(1);
    assert_eq!(bad.validate(), Err(Error::Disk));
    bad = independent;
    bad.vms[0].as_mut().unwrap().disk = Some(4);
    assert_eq!(bad.validate(), Err(Error::Disk));
    bad = independent;
    bad.disks[1] = None;
    assert_eq!(bad.validate(), Err(Error::Disk));
    bad = independent;
    bad.disks[0].as_mut().unwrap().sectors = 0;
    assert_eq!(bad.validate(), Err(Error::Disk));
    bad = independent;
    bad.disks[0].as_mut().unwrap().sectors = u64::MAX;
    assert_eq!(bad.validate(), Err(Error::Disk));
    bad = independent;
    bad.vms[0].as_mut().unwrap().disk = None;
    bad.vms[0].as_mut().unwrap().writable = false;
    assert_eq!(bad.validate(), Err(Error::Disk)); // Unassigned native backing.
}
#[test]
fn only_explicit_unassigned_managed_volumes_are_delegable() {
    let mut cfg = fixture();
    cfg.vms[1..].fill(None);
    cfg.management = true;
    for writable in [false, true] {
        cfg.disks[0] = Some(DiskConfig {
            sectors: 64,
            writable,
            managed: true,
        });
        let bytes = cfg.encode().unwrap();
        let disk = BootConfig::decode(&bytes).unwrap().disks[0].unwrap();
        assert!(disk.managed && disk.writable == writable);
        let mut bad_flags = bytes;
        bad_flags[crate::DISKS_OFFSET + 8] |= 4;
        assert!(matches!(
            BootConfig::decode(&bad_flags),
            Err(Error::Reserved)
        ));
        cfg.vms[0].as_mut().unwrap().disk = Some(0);
        assert_eq!(cfg.validate(), Err(Error::Disk));
        cfg.vms[0].as_mut().unwrap().disk = None;
        cfg.management = false;
        assert_eq!(cfg.validate(), Err(Error::Disk));
        cfg.management = true;
        cfg.disks[0].as_mut().unwrap().managed = false;
        assert_eq!(cfg.validate(), Err(Error::Disk));
    }
}
