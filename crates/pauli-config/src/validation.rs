//! Validate untrusted manifest ranges and shared-resource assignments once.
//! Ownership/Lock Ordering: immutable caller-owned data; no locks or I/O.
//! Invariants: accepted RAM/image ranges cannot overflow or alias private RAM.

use super::*;

impl BootConfig {
    pub fn count(&self) -> usize {
        self.vms.iter().flatten().count()
    }

    /// Resolve all explicit reservations against the actual management hart and host width.
    pub fn exclusive_owner_mask(&self, boot_cpu: usize, host_cpus: usize) -> Result<u64, Error> {
        let host = u8::try_from(host_cpus).map_err(|_| Error::Affinity)?;
        let reserved = self.exclusive_pins(host)?;
        let eligible = vm::eligible_owners(boot_cpu, host_cpus, reserved)?;
        if eligible & !(1u64 << boot_cpu) == 0
            && self
                .vms
                .iter()
                .flatten()
                .any(|vm| vm.cpu_affinity == CpuAffinity::Workers)
        {
            return Err(Error::Affinity);
        }
        Ok(reserved)
    }

    /// Static reservations are meaningful before the boot hart or actual width is known.
    fn exclusive_pins(&self, host_cpus: u8) -> Result<u64, Error> {
        let mut reserved = 0;
        let mut shared = 0;
        for vm in self.vms.iter().flatten() {
            let pins = vm.validated_pins(host_cpus)?;
            if reserved & pins != 0 || (vm.exclusive_cpus && shared & pins != 0) {
                return Err(Error::Affinity);
            }
            if vm.exclusive_cpus {
                reserved |= pins;
            } else {
                shared |= pins;
            }
        }
        Ok(reserved)
    }

    /// External-format boundary. Board owners additionally validate physical
    /// ownership/limits before reserving backing or dispatching any vCPU.
    pub fn validate(&self) -> Result<(), Error> {
        self.validate_header()?;
        self.validate_disks()?;
        self.exclusive_pins(MAX_HOST_CPUS)?;
        let mut assigned = [0; MAX_DISKS];
        for (index, vm) in self.vms.iter().flatten().enumerate() {
            self.validate_memory(vm)?;
            self.validate_image(vm)?;
            if vm.dtb_size == 0
                || vm.dtb_size > MAX_GUEST_DTB as u64
                || vm.dtb_offset.checked_add(vm.dtb_size).ok_or(Error::Dtb)? > self.dtb_size
            {
                return Err(Error::Dtb);
            }
            if !(100..=10_000).contains(&vm.quantum_us) {
                return Err(Error::Quantum);
            }
            self.validate_network(index, vm)?;
            self.assign_disk(vm, &mut assigned)?;
            for other in self.vms[..index].iter().flatten() {
                validate_pair(vm, other)?;
            }
        }
        for (slot, disk) in self.disks.iter().flatten().enumerate() {
            // Missing boot assignments are not implicit management authority.
            let valid = if disk.managed {
                self.management && assigned[slot] == 0
            } else {
                assigned[slot] != 0 && (!disk.writable || assigned[slot] == 1)
            };
            if !valid {
                return Err(Error::Disk);
            }
        }
        Ok(())
    }

    fn validate_header(&self) -> Result<(), Error> {
        let count = self.count();
        if self.vms.iter().flatten().filter(|vm| vm.net_uplink).count() > 1 {
            return Err(Error::Network);
        }
        if count == 0 || self.vms[..count].iter().any(Option::is_none) {
            return Err(Error::Count);
        }
        if count > 1 && !self.management {
            return Err(Error::Count);
        }
        self.template_base
            .checked_add(self.template_size)
            .ok_or(Error::Image)?;
        if self.template_base % 4096 != 0
            || self.template_size == 0
            || self.template_size % 4096 != 0
            || self.dtb_size == 0
            || self.dtb_size > 64 * 1024
        {
            return Err(Error::Image);
        }
        Ok(())
    }

    fn validate_disks(&self) -> Result<(), Error> {
        let count = self.disks.iter().flatten().count();
        if self.disks[..count].iter().any(Option::is_none) {
            return Err(Error::Disk);
        }
        for disk in self.disks.iter().flatten() {
            if disk.sectors == 0 || disk.sectors > 128 * 1024 * 1024 * 1024 / 512 {
                return Err(Error::Disk);
            }
        }
        Ok(())
    }

    fn validate_memory(&self, vm: &VmConfig) -> Result<(), Error> {
        if VmConfig::new(vm.name())?.name != vm.name {
            return Err(Error::Name);
        }
        vm.validate_guest_geometry()?;
        let alignment = if vm.firmware.is_some() {
            FIRMWARE_PAGE
        } else {
            ALIGN
        };
        if vm.host_base % alignment != 0 || vm.host_base.checked_add(vm.memory_size).is_none() {
            return Err(Error::Memory);
        }
        if overlaps(
            vm.host_base,
            vm.memory_size,
            self.template_base,
            self.template_size,
        ) {
            return Err(Error::Memory);
        }
        Ok(())
    }

    fn validate_image(&self, vm: &VmConfig) -> Result<(), Error> {
        let end = self.template_base + self.template_size; // validate_header checked overflow.
        let image = vm.image;
        let minimum = if vm.firmware.is_some() { 1 } else { 64 };
        if !(minimum..=MAX_KERNEL_BYTES).contains(&image.kernel_size)
            || image.kernel_source % 4096 != 0
            || !contained(
                self.template_base,
                end,
                image.kernel_source,
                image.kernel_size,
            )
            || (image.initrd_size == 0 && image.initrd_source != 0)
            || (image.initrd_size != 0
                && (image.initrd_source % 4096 != 0
                    || !contained(
                        self.template_base,
                        end,
                        image.initrd_source,
                        image.initrd_size,
                    )))
            || overlaps(
                image.kernel_source,
                image.kernel_size,
                image.initrd_source,
                image.initrd_size,
            )
        {
            return Err(Error::Image);
        }
        Ok(())
    }

    fn validate_network(&self, index: usize, vm: &VmConfig) -> Result<(), Error> {
        if let Some(peer) = vm.net_peer {
            if vm.net_uplink
                || usize::from(peer) == index
                || self
                    .vms
                    .get(usize::from(peer))
                    .and_then(Option::as_ref)
                    .is_none_or(|other| other.net_peer != Some(index as u8))
            {
                return Err(Error::Network);
            }
        }
        Ok(())
    }

    fn assign_disk(&self, vm: &VmConfig, assigned: &mut [u8; MAX_DISKS]) -> Result<(), Error> {
        match vm.disk {
            Some(slot) => {
                let disk = self
                    .disks
                    .get(usize::from(slot))
                    .and_then(Option::as_ref)
                    .ok_or(Error::Disk)?;
                if vm.writable && !disk.writable {
                    return Err(Error::Disk);
                }
                assigned[usize::from(slot)] += 1;
            }
            None if vm.writable => return Err(Error::Disk),
            None => {}
        }
        Ok(())
    }
}

fn validate_pair(vm: &VmConfig, other: &VmConfig) -> Result<(), Error> {
    if vm.name == other.name {
        return Err(Error::Name);
    }
    if overlaps(
        vm.host_base,
        vm.memory_size,
        other.host_base,
        other.memory_size,
    ) {
        return Err(Error::Memory);
    }
    let image = vm.image;
    let previous = other.image;
    for (a, al, b, bl, identical_kind) in [
        (
            image.kernel_source,
            image.kernel_size,
            previous.kernel_source,
            previous.kernel_size,
            true,
        ),
        (
            image.initrd_source,
            image.initrd_size,
            previous.initrd_source,
            previous.initrd_size,
            true,
        ),
        (
            image.kernel_source,
            image.kernel_size,
            previous.initrd_source,
            previous.initrd_size,
            false,
        ),
        (
            image.initrd_source,
            image.initrd_size,
            previous.kernel_source,
            previous.kernel_size,
            false,
        ),
    ] {
        if overlaps(a, al, b, bl) && !(identical_kind && a == b && al == bl) {
            return Err(Error::Image);
        }
    }
    if image.kernel_source == previous.kernel_source
        && image.kernel_memory_size != previous.kernel_memory_size
    {
        return Err(Error::Image);
    }
    Ok(())
}

fn contained(base: u64, end: u64, address: u64, size: u64) -> bool {
    address >= base && address.checked_add(size).is_some_and(|limit| limit <= end)
}

/// Call only for previously checked, non-overflowing ranges.
pub fn overlaps(a: u64, al: u64, b: u64, bl: u64) -> bool {
    al != 0 && bl != 0 && a < b + bl && b < a + al
}
