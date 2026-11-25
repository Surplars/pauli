//! Private cold-boot image restoration; immutable templates never map to guests.
//! Ownership/Lock Ordering: VM is frozen Joined; device/RAM owner is exclusive.
//! Invariants: checked image slices fit private RAM; TLB retirement precedes clearing.
use crate::{
    HvError, Result,
    arch::VcpuBoot,
    board::{BootImages, Payload},
    memory::{GuestRam, RamImage},
};
use pauli_config::{BootConfig, KERNEL_OFFSET, VmConfig};
use pauli_hal::addr::PAGE_SIZE;

#[cfg(test)]
mod tests;

pub(crate) fn validate_embedded(config: &BootConfig) -> Result<()> {
    let vm = config.vms[0].ok_or(HvError::InvalidConfig)?;
    let page = PAGE_SIZE as u64;
    if config.count() != 1
        || config.disks.iter().any(Option::is_some)
        || vm.disk.is_some()
        || vm.writable
        || vm.net_peer.is_some()
        || vm.net_uplink
        || vm.vcpus != 1
        || vm.dtb_size != 0
        || vm.image.initrd_size != 0
        || !(1..=page).contains(&vm.image.kernel_size)
        || vm.image.kernel_memory_size != vm.image.kernel_size
        || !(page..=16 * page).contains(&vm.memory_size)
        || [vm.host_base, vm.guest_base, vm.memory_size]
            .iter()
            .any(|n| n % page != 0)
        || vm.guest_base.checked_add(vm.memory_size).is_none()
        || !(100..=10_000).contains(&vm.quantum_us)
    {
        return Err(HvError::InvalidConfig);
    }
    Ok(())
}
pub(super) fn boot(
    config: &VmConfig,
    payload: Payload,
    timebase: u64,
    sstc: bool,
) -> Result<VcpuBoot> {
    let payload = match (payload, config.firmware) {
        (Payload::Linux, layout) => layout.map_or(Payload::Linux, Payload::Firmware),
        (Payload::Embedded, None) => Payload::Embedded,
        _ => return Err(HvError::InvalidConfig),
    };
    let (entry, opaque) = match payload {
        Payload::Linux => (
            config.guest_base + KERNEL_OFFSET,
            config.guest_base + config.guest_dtb_offset(),
        ),
        Payload::Embedded => (config.guest_base, timebase),
        Payload::Firmware(_) => (
            config.guest_base,
            config.guest_base + config.guest_dtb_offset(),
        ),
    };
    Ok(VcpuBoot {
        entry: usize::try_from(entry).map_err(|_| HvError::InvalidImage)?,
        opaque: usize::try_from(opaque).map_err(|_| HvError::InvalidImage)?,
        payload,
        sstc,
    })
}
pub(super) fn prepare(
    config: &VmConfig,
    images: BootImages,
    ram: &GuestRam,
    payload: Payload,
) -> Result<RamImage> {
    if images.kernel.len() as u64 != config.image.kernel_size
        || images.initrd.len() as u64 != config.image.initrd_size
        || images.dtb.len() as u64 != config.dtb_size
        || ram.guest_base() as u64 != config.guest_base
        || ram.host_range().base() as u64 != config.host_base
        || ram.len() as u64 != config.memory_size
    {
        return Err(HvError::InvalidImage);
    }
    match payload {
        Payload::Linux => ram.plan_image([
            (KERNEL_OFFSET, images.kernel),
            (config.initrd_offset(), images.initrd),
            (config.guest_dtb_offset(), images.dtb),
        ]),
        Payload::Embedded => ram.plan_image([(0, images.kernel), (0, &[]), (0, &[])]),
        Payload::Firmware(_) => ram.plan_image([
            (0, images.kernel),
            (0, &[]),
            (config.guest_dtb_offset(), images.dtb),
        ]),
    }
}
