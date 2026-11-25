//! Compact firmware and mixed boot records at the untrusted manifest boundary.
//! Ownership/Lock Ordering: fixture-local mutations; no hardware or VM state.
//! Invariants: no inherited DMA/initrd, no Linux offsets in small RAM, no RAM aliases.
use super::*;
use crate::{FIRMWARE_PAGE as P, FirmwareLayout, MAX_FIRMWARE_RAM, MAX_GUEST_DTB};
fn pair() -> BootConfig {
    let mut c = fixture();
    c.vms[2..].fill(None);
    let v = c.vms[0].as_mut().unwrap();
    v.firmware = Some(FirmwareLayout {
        rx_pages: 2,
        ro_pages: 1,
    });
    v.memory_size = 32 * 1024;
    v.host_base += P;
    v.image.kernel_source += 1 << 20;
    v.image.kernel_size = P + 16;
    v.image.kernel_memory_size = 3 * P;
    c
}
#[test]
fn small_firmware_roundtrips_beside_linux() {
    let cfg = pair();
    let decoded = BootConfig::decode(&cfg.encode().unwrap()).unwrap();
    let vm = decoded.vms[0].unwrap();
    assert_eq!(vm.firmware, cfg.vms[0].unwrap().firmware);
    assert_eq!(vm.initrd_offset(), 0);
    assert_eq!(vm.guest_dtb_offset(), vm.memory_size - MAX_GUEST_DTB as u64);
    let mut changed = vm;
    changed.firmware = None;
    assert_eq!(changed.validate_guest_geometry(), Err(Error::Memory));
    assert!(decoded.vms[1].unwrap().firmware.is_none());
}
#[test]
fn reciprocal_fixed_peer_roundtrips_between_firmware_and_linux() {
    let mut cfg = pair();
    cfg.vms[0].as_mut().unwrap().net_peer = Some(1);
    assert_eq!(cfg.validate(), Err(Error::Network));
    cfg.vms[1].as_mut().unwrap().net_peer = Some(0);
    let decoded = BootConfig::decode(&cfg.encode().unwrap()).unwrap();
    assert_eq!(
        decoded.vms[0].unwrap().firmware,
        cfg.vms[0].unwrap().firmware
    );
    assert_eq!(decoded.vms[0].unwrap().net_peer, Some(1));
    assert_eq!(decoded.vms[1].unwrap().net_peer, Some(0));
}

#[test]
fn leased_net_is_runtime_only_and_uses_the_same_linux_or_firmware_device() {
    use crate::{GuestBoardLayout, NetLease, guest_dtb::riscv_configured};
    use pauli_hal::fdt::Fdt;

    let layout = GuestBoardLayout::DEFAULT;
    let cfg = pair();
    for (index, vm) in cfg.vms.iter().flatten().enumerate() {
        let mut child = vm.renamed("child").unwrap();
        assert!(!child.has_net());
        let path = format!("/virtio@{:x}", layout.net.base);
        let mut out = [0; MAX_GUEST_DTB];
        let len = riscv_configured(&child, layout, 1, "", false, &mut out).unwrap();
        assert!(
            Fdt::new(&out[..len])
                .unwrap()
                .find(&path)
                .unwrap()
                .is_none()
        );
        let lease = NetLease::new(3, 1).unwrap();
        child.net_lease = Some(lease);
        child.validate_guest_geometry().unwrap();
        assert!(child.has_net() && !child.renamed("plain").unwrap().has_net());
        let len = riscv_configured(&child, layout, 1, "", false, &mut out).unwrap();
        let fdt = Fdt::new(&out[..len]).unwrap();
        let node = fdt.find(&path).unwrap().unwrap();
        assert_eq!(
            node.reg().unwrap(),
            Some((layout.net.base, layout.net.size))
        );
        assert_eq!(node.u32("interrupts").unwrap(), Some(layout.net_irq));
        let mut manifest = cfg;
        manifest.vms[index].as_mut().unwrap().net_lease = Some(lease);
        assert_eq!(manifest.validate(), Err(Error::Network));
        assert_eq!(manifest.encode(), Err(Error::Network));
        for (net_peer, net_uplink) in [(Some(0), false), (None, true)] {
            child.net_peer = net_peer;
            child.net_uplink = net_uplink;
            assert_eq!(child.validate_guest_geometry(), Err(Error::Network));
        }
        child.net_peer = None;
        child.net_uplink = false;
        for lease in [
            NetLease {
                channel: 4,
                side: 0,
            },
            NetLease {
                channel: 0,
                side: 2,
            },
        ] {
            child.net_lease = Some(lease);
            assert_eq!(child.validate_guest_geometry(), Err(Error::Network));
            assert_eq!(
                riscv_configured(&child, layout, 1, "", false, &mut out),
                Err(Error::Network)
            );
        }
    }
}
#[test]
fn invalid_permissions_shapes_and_sources_never_enter_the_manifest() {
    for change in [
        (|v: &mut VmConfig| v.memory_size -= 1) as fn(&mut VmConfig),
        |v| v.memory_size = MAX_FIRMWARE_RAM + P,
        |v| v.firmware.as_mut().unwrap().rx_pages = 0,
        |v| v.firmware.as_mut().unwrap().ro_pages = 3,
        |v| v.image.kernel_memory_size = v.guest_dtb_offset() + 1,
        |v| v.image.kernel_size = v.image.kernel_memory_size + 1,
        |v| v.image.initrd_size = 1,
        |v| v.disk = Some(0),
        |v| v.writable = true,
        |v| v.net_uplink = true,
        |v| v.net_peer = Some(1),
        |v| v.vcpus = 2,
        |v| v.host_base += 1,
        |v| v.guest_base = u64::MAX - (P - 1),
    ] {
        let mut cfg = pair();
        change(cfg.vms[0].as_mut().unwrap());
        assert!(cfg.encode().is_err());
    }
    let mut c = pair();
    c.vms[0].as_mut().unwrap().host_base = c.vms[1].unwrap().host_base + P;
    assert_eq!(c.validate(), Err(Error::Memory));
}
