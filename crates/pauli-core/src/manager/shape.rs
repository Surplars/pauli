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

pub(super) fn config(
    source: &VmConfig,
    spec: ChildSpec,
    boot_cpu: usize,
    host_cpus: usize,
    exclusive_owners: u64,
) -> Result<VmConfig> {
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
            .map_err(|_| HvError::InvalidConfig)?;
    }
    child
        .validate_placement_excluding(boot_cpu, host_cpus, exclusive_owners)
        .map_err(|_| HvError::InvalidConfig)?;
    Ok(child)
}

#[cfg(test)]
#[test]
fn unshaped_worker_clone_requires_an_unreserved_worker() {
    let mut template = VmConfig::new("template").unwrap();
    template.cpu_affinity = CpuAffinity::Workers;
    let spec = pauli_control::ChildName::new("child").unwrap().into();
    assert!(config(&template, spec, 1, 3, 0b100).is_ok());
    assert_eq!(
        config(&template, spec, 1, 3, 0b101).err(),
        Some(HvError::InvalidConfig)
    );
}

#[derive(Clone, Copy)]
pub(super) struct DtbChange {
    pub source_sstc: bool,
    pub target_sstc: bool,
    /// An untouched, opaque baseline is still safe on Sstc-capable owners.
    pub optional: bool,
}

pub(super) fn rebuild<A: HvArch, B: Board>(
    board: &B,
    source: &VmConfig,
    child: &mut VmConfig,
    resources: &mut VmResources,
    change: DtbChange,
) -> Result<()> {
    match render::<A, B>(board, source, child, resources, change) {
        Err(HvError::Unsupported | HvError::InvalidImage) if change.optional => Ok(()),
        result => result,
    }
}

fn render<A: HvArch, B: Board>(
    board: &B,
    source: &VmConfig,
    child: &mut VmConfig,
    resources: &mut VmResources,
    change: DtbChange,
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
    let original_size = if change.source_sstc {
        A::write_extended_linux_dtb(source, layout, hz, args, out)?
    } else {
        A::write_linux_dtb(source, layout, hz, args, out)?
    };
    if out.get(..original_size) != Some(original) {
        return Err(HvError::Unsupported);
    }
    let render = if change.target_sstc {
        A::write_extended_linux_dtb
    } else {
        A::write_linux_dtb
    };
    let len = render(child, layout, hz, args, out)?;
    resources.images.dtb = out.get(..len).ok_or(HvError::InvalidImage)?;
    child.dtb_size = len as u64;
    Ok(())
}
