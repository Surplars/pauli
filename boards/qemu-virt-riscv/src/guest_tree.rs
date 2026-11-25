//! Adapt only a launcher's canonical Linux tree to a verified native timer.
//! Board-specific nodes, reservations and boot arguments are retained verbatim.
use pauli_config::{GuestBoardLayout, VmConfig, guest_dtb};
use pauli_hal::fdt::Fdt;

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
    let plain = guest_dtb::riscv(vm, layout, hz, args, out).ok()?;
    if out.get(..plain)? != original {
        return None;
    }
    guest_dtb::riscv_with_sstc(vm, layout, hz, args, true, out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pauli_config::MAX_GUEST_DTB;

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
}
