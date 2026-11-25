//! Adapt only a launcher's canonical Linux tree to a verified native timer.
//! Board-specific nodes, reservations and boot arguments are retained verbatim.
use pauli_config::{BootConfig, Error, GuestBoardLayout, MAX_BOOT_VMS, VmConfig, guest_dtb};
use pauli_hal::fdt::Fdt;

/// Decide which Linux trees can advertise Sstc on their actual vCPU owners.
pub fn sstc_upgrades(
    config: &BootConfig,
    boot: usize,
    host_cpus: usize,
    capable: impl Fn(usize) -> bool,
) -> Result<[bool; MAX_BOOT_VMS], Error> {
    let excluded = config.exclusive_owner_mask(boot, host_cpus)?;
    Ok(core::array::from_fn(|slot| {
        config.vms[slot].as_ref().is_some_and(|vm| {
            vm.firmware.is_none()
                && capable(vm.owner_mask_excluding(slot, boot, host_cpus, excluded))
        })
    }))
}

/// Returns the extended length when the original is the canonical baseline.
/// The caller keeps the original bytes for any other tree or insufficient space.
pub fn enable_sstc(vm: &VmConfig, original: &[u8], hz: u32, out: &mut [u8]) -> Option<usize> {
    if vm.firmware.is_some() {
        return None;
    }
    let tree = Fdt::new(original).ok()?;
    let chosen = tree.find("/chosen").ok().flatten()?;
    let args = chosen.text("bootargs").ok().flatten()?;
    let layout = GuestBoardLayout::DEFAULT;
    let plain = guest_dtb::riscv_configured(vm, layout, hz, args, false, out).ok()?;
    if out.get(..plain)? != original {
        return None;
    }
    guest_dtb::riscv_configured(vm, layout, hz, args, true, out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pauli_config::MAX_GUEST_DTB;

    #[test]
    fn loader_checks_the_same_exclusive_automatic_and_worker_owners_as_dispatch() {
        use pauli_config::{CpuAffinity, FirmwareLayout, MAX_DISKS, MAX_GUEST_VCPUS};

        let mut rt = VmConfig::new("rt").unwrap();
        let mut pins = [0; MAX_GUEST_VCPUS as usize];
        pins[0] = 3;
        rt.cpu_affinity = CpuAffinity::Physical(pins);
        rt.exclusive_cpus = true;
        let auto = VmConfig::new("auto").unwrap();
        let mut workers = VmConfig::new("workers").unwrap();
        workers.cpu_affinity = CpuAffinity::Workers;
        workers.vcpus = 2;
        let mut config = BootConfig {
            template_base: 0,
            template_size: 0,
            dtb_size: 0,
            management: true,
            smoke_payload: false,
            disks: [None; MAX_DISKS],
            vms: [Some(rt), Some(auto), Some(workers), None],
        };
        // Boot owner 2; automatic order is [2, 0, 1], workers order is [0, 1].
        for (capable, expected) in [
            (0b0001, [false, true, false, false]),
            (0b0011, [false, true, true, false]),
            (0b1000, [true, false, false, false]),
        ] {
            assert_eq!(
                sstc_upgrades(&config, 2, 4, |owners| owners & capable == owners),
                Ok(expected)
            );
        }
        config.vms[0].as_mut().unwrap().firmware = Some(FirmwareLayout {
            rx_pages: 1,
            ro_pages: 0,
        });
        assert_eq!(
            sstc_upgrades(&config, 2, 4, |_| true),
            Ok([false, true, true, false])
        );
        assert_eq!(
            sstc_upgrades(&config, 3, 4, |_| panic!(
                "bad topology reached timer admission"
            )),
            Err(Error::Affinity)
        );
    }

    #[test]
    fn canonical_upgrade_preserves_bootargs_and_every_vcpu() {
        let mut vm = VmConfig::new("primary").unwrap();
        vm.vcpus = 2;
        vm.aia = true;
        let args = "console=ttyS0 init=/probe";
        let hz = 10_000_000;
        let mut baseline = [0; MAX_GUEST_DTB];
        let mut expected = [0; MAX_GUEST_DTB];
        let mut out = [0; MAX_GUEST_DTB];
        let layout = GuestBoardLayout::DEFAULT;
        let plain = guest_dtb::riscv(&vm, layout, hz, args, &mut baseline).unwrap();
        let extended =
            guest_dtb::riscv_with_sstc(&vm, layout, hz, args, true, &mut expected).unwrap();
        assert_eq!(
            enable_sstc(&vm, &baseline[..plain], hz, &mut out),
            Some(extended)
        );
        assert_eq!(out[..extended], expected[..extended]);
        assert_eq!(enable_sstc(&vm, &expected[..extended], hz, &mut out), None);

        // A valid guest tree carrying custom model data must survive staging byte-for-byte.
        let offset = baseline[..plain]
            .windows(7)
            .position(|v| v == b"OpenIon")
            .unwrap();
        baseline[offset] = b'X';
        assert!(Fdt::new(&baseline[..plain]).is_ok());
        assert_eq!(enable_sstc(&vm, &baseline[..plain], hz, &mut out), None);
    }

    #[test]
    fn canonical_control_tree_keeps_its_event_node_through_sstc_upgrade() {
        let mut vm = VmConfig::new("peer").unwrap();
        vm.control_events = true;
        let layout = GuestBoardLayout::DEFAULT;
        let (mut baseline, mut upgraded) = ([0; MAX_GUEST_DTB], [0; MAX_GUEST_DTB]);
        let n = guest_dtb::riscv_configured(&vm, layout, 1, "", false, &mut baseline).unwrap();
        let used = enable_sstc(&vm, &baseline[..n], 1, &mut upgraded).unwrap();
        assert_eq!(guest_dtb::riscv_sstc(&upgraded[..used], 1), Ok(true));
        assert_eq!(
            guest_dtb::validate_control_events(&upgraded[..used], &vm, layout),
            Ok(())
        );
        vm.control_events = false;
        assert!(enable_sstc(&vm, &baseline[..n], 1, &mut upgraded).is_none());
    }
}
