//! Stage the same virtual board description used when the running manager provisions a VM.
//! Ownership/Lock Ordering: one launch owns its bounded output; no native I/O or locks.
//! Invariants: only a finished tree is returned, never an incomplete backing buffer.
use anyhow::{Result, anyhow};
use pauli_config::{GuestBoardLayout, MAX_GUEST_DTB, VmConfig};

pub fn guest(vm: &VmConfig, layout: GuestBoardLayout, hz: u32, args: &str) -> Result<Vec<u8>> {
    let mut out = vec![0; MAX_GUEST_DTB];
    let used = pauli_config::guest_dtb::riscv(vm, layout, hz, args, &mut out)
        .map_err(|e| anyhow!("guest DTB: {e:?}"))?;
    out.truncate(used);
    Ok(out)
}

#[cfg(test)]
#[test]
fn shared_layout_clock_and_selected_irq_topology() {
    use sha2::{Digest, Sha256};
    let mut layout = GuestBoardLayout::DEFAULT;
    layout.uart.base = 0x1600_0000;
    let mut vm = VmConfig::new("dtb").unwrap();
    vm.disk = Some(0);
    vm.vcpus = 2;
    // Captured from the original host-only encoder before it was replaced.
    for (aia, digest) in [
        (
            false,
            "71542fd2ca9c1231bfa06adec572a16a1690087b3d10c24a36529e011d10dce6",
        ),
        (
            true,
            "e5b9015ef4e39d7bf39bc8391b945b5c922fb4676f789c88019e6c7e1c91b8f3",
        ),
    ] {
        vm.aia = aia;
        let bytes = guest(&vm, layout, 40_000_000, "console=ttyS0").unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(&bytes)), digest);
        let tree = pauli_hal::fdt::Fdt::new(&bytes).unwrap();
        let cpus = tree.find("/cpus").unwrap().unwrap();
        assert_eq!(cpus.u32("timebase-frequency").unwrap(), Some(40_000_000));
        let uart = tree.find("/uart@16000000").unwrap().unwrap();
        assert_eq!(
            uart.reg().unwrap(),
            Some((layout.uart.base, layout.uart.size))
        );
        assert_eq!(tree.find("/imsics@28000000").unwrap().is_some(), aia);
        assert!(tree.find("/virtio@10001000").unwrap().is_some());
        assert!(tree.find("/virtio@10002000").unwrap().is_none());
    }
}
