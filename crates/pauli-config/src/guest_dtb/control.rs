//! Verify just the optional control device in the tree actually given to VS.
use super::{CONTROL_COMPATIBLE, LEVEL_HIGH};
use crate::{Error, GuestBoardLayout, VmConfig};
use pauli_hal::fdt::{self, Fdt};

pub fn validate_control_events(
    blob: &[u8],
    vm: &VmConfig,
    layout: GuestBoardLayout,
) -> Result<(), Error> {
    check(blob, vm, layout).map_err(|_| Error::Dtb)
}

fn check(blob: &[u8], vm: &VmConfig, layout: GuestBoardLayout) -> fdt::Result<()> {
    let tree = Fdt::new(blob)?;
    let mut parent = None;
    for node in tree.nodes() {
        let node = node?;
        // Opaque trees without this binding retain their unrelated properties.
        if !node.property("compatible")?.is_some_and(|p| {
            p.split(|&b| b == 0)
                .any(|s| s == CONTROL_COMPATIBLE.as_bytes())
        }) {
            continue;
        }
        if !vm.control_events
            || parent.is_some()
            || node.depth() != 1
            || !matches!(
                node.unique_property("status")?,
                None | Some(b"ok\0" | b"okay\0")
            )
            || node
                .unique_property("compatible")?
                .is_none_or(|p| !p.ends_with(&[0]))
            || node.property("reg")?.is_some()
            || node.property("interrupts-extended")?.is_some()
        {
            return Err(fdt::Error::Property);
        }
        let expected =
            ((u64::from(layout.control_irq) << 32) | u64::from(LEVEL_HIGH)).to_be_bytes();
        let length = if vm.aia { 8 } else { 4 };
        if node.unique_property("interrupts")? != Some(&expected[..length]) {
            return Err(fdt::Error::Property);
        }
        let phandle: [u8; 4] = node
            .unique_property("interrupt-parent")?
            .ok_or(fdt::Error::Cells)?
            .try_into()
            .map_err(|_| fdt::Error::Cells)?;
        if matches!(u32::from_be_bytes(phandle), 0 | u32::MAX) {
            return Err(fdt::Error::Cells);
        }
        parent = Some(phandle);
    }
    let Some(phandle) = parent else {
        return if vm.control_events {
            Err(fdt::Error::Property)
        } else {
            Ok(())
        };
    };
    let (region, compatible, cells) = if vm.aia {
        (layout.aplic, "riscv,aplic", 2u32)
    } else {
        (layout.plic, "riscv,plic0", 1u32)
    };
    let mut found = false;
    for node in tree.nodes() {
        let node = node?;
        let p = node.property("phandle")?;
        let linux_p = node.property("linux,phandle")?;
        if p != Some(&phandle) && linux_p != Some(&phandle) {
            continue;
        }
        if found
            || node.depth() != 1
            || !matches!(
                node.unique_property("status")?,
                None | Some(b"ok\0" | b"okay\0")
            )
            || node.unique_property("compatible")?.is_none()
            || !node.compatible(compatible)?
            || node
                .unique_property("phandle")?
                .is_some_and(|v| v != phandle)
            || node
                .unique_property("linux,phandle")?
                .is_some_and(|v| v != phandle)
            || node.unique_property("interrupt-controller")? != Some(&[])
            || node.property("interrupt-map")?.is_some()
            || node.unique_property("#interrupt-cells")? != Some(&cells.to_be_bytes())
            || node.unique_property("reg")?.is_none()
            || node.reg()? != Some((region.base, region.size))
        {
            return Err(fdt::Error::Property);
        }
        found = true;
    }
    if found {
        Ok(())
    } else {
        Err(fdt::Error::Property)
    }
}
