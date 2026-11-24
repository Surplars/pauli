//! Resolve a creation request and rebuild the private Linux discovery image.
//! Ownership/Lock Ordering: sole provisioner; private output never aliases template or guest.
//! Invariants: optional fields cannot widen physical affinity; opaque trees are never rewritten.
use crate::{
    HvError, Result,
    arch::HvArch,
    board::{Board, VmResources},
};
use pauli_config::{CpuAffinity, MAX_GUEST_DTB, VmConfig};
use pauli_control::ChildSpec;
use pauli_hal::fdt::Fdt;

pub(super) fn config(source: &VmConfig, spec: ChildSpec, host_cpus: u8) -> Result<VmConfig> {
    let mut child = source
        .renamed(spec.name.as_str())
        .map_err(|_| HvError::InvalidConfig)?;
    if let Some(cpus) = spec.shape.vcpus {
        child.vcpus = cpus.get();
        if let CpuAffinity::Physical(ref mut owners) = child.cpu_affinity {
            // Unused entries are padding, not permission to run additional CPUs on hart 0.
            if child.vcpus > source.vcpus {
                return Err(HvError::Unsupported);
            }
            owners[usize::from(child.vcpus)..].fill(0);
        }
    }
    if let Some(mib) = spec.shape.memory_mib {
        child.memory_size = u64::from(mib.get()) << 20;
    }
    if spec.shape != Default::default() {
        child
            .validate_guest_geometry()
            .and_then(|()| child.validate_placement(host_cpus))
            .map_err(|_| HvError::InvalidConfig)?;
    }
    Ok(child)
}

pub(super) fn rebuild<A: HvArch, B: Board>(
    board: &B,
    source: &VmConfig,
    child: &mut VmConfig,
    resources: &mut VmResources,
) -> Result<()> {
    let original = resources.images.dtb;
    let tree = Fdt::new(original).map_err(|_| HvError::InvalidImage)?;
    let args = tree
        .find("/chosen")
        .map_err(|_| HvError::InvalidImage)?
        .ok_or(HvError::Unsupported)?
        .text("bootargs")
        .map_err(|_| HvError::InvalidImage)?
        .ok_or(HvError::Unsupported)?;
    let hz = u32::try_from(board.info().timebase_hz).map_err(|_| HvError::InvalidConfig)?;
    let out = resources.arena.take(MAX_GUEST_DTB, 8)?;
    let layout = board.layout();
    // Recreate the original first. Extra firmware reservations, custom nodes or
    // boot policy must not silently disappear when we change the CPU/RAM shape.
    let old_len = A::write_linux_dtb(source, layout, hz, args, out)?;
    if out.get(..old_len) != Some(original) {
        return Err(HvError::Unsupported);
    }
    let len = A::write_linux_dtb(child, layout, hz, args, out)?;
    resources.images.dtb = out.get(..len).ok_or(HvError::InvalidImage)?;
    child.dtb_size = len as u64;
    Ok(())
}
