//! VM names, placement and validated guest image offsets.
//! Ownership/Lock Ordering: exclusive caller ownership; no locks in this module.
//! Invariants: names are private ASCII; offset/owner helpers require validated input.

use super::*;

impl VmConfig {
    pub fn new(name: &str) -> Result<Self, Error> {
        if name.is_empty()
            || name.len() > NAME_MAX
            || !name.as_bytes()[0].is_ascii_alphabetic()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(Error::Name);
        }
        let mut raw = [0; NAME_MAX];
        raw[..name.len()].copy_from_slice(name.as_bytes());
        Ok(Self {
            name: raw,
            guest_base: GuestBoardLayout::DEFAULT.ram_base,
            host_base: 0,
            memory_size: 96 * 1024 * 1024,
            dtb_offset: 0,
            dtb_size: 0,
            quantum_us: 2000,
            autostart: false,
            disk: None,
            writable: false,
            net_peer: None,
            net_uplink: false,
            stop_on_reset: false,
            aia: false,
            vcpus: 1,
            cpu_affinity: CpuAffinity::Automatic,
            image: VmImage::default(),
            firmware: None,
        })
    }
    pub fn name(&self) -> &str {
        let end = self.name.iter().position(|b| *b == 0).unwrap_or(NAME_MAX);
        // Private storage is constructed only by new() or canonical decoding;
        // both reject non-ASCII names before installing the value.
        core::str::from_utf8(&self.name[..end]).unwrap()
    }
    /// Keep the trusted boot geometry and image; validate the new resource name.
    pub fn renamed(&self, name: &str) -> Result<Self, Error> {
        Ok(Self {
            name: Self::new(name)?.name,
            autostart: false,
            ..*self
        })
    }
    /// The manifest and runtime requests use the same fit rules before computing
    /// offsets. Linux's loaded kernel (including BSS), initrd and DTB may not alias.
    pub fn validate_guest_geometry(&self) -> Result<(), Error> {
        if let Some(layout) = self.firmware {
            let dtb = MAX_GUEST_DTB as u64;
            if !(dtb + 2 * FIRMWARE_PAGE..=MAX_FIRMWARE_RAM).contains(&self.memory_size)
                || !self.memory_size.is_multiple_of(FIRMWARE_PAGE)
                || !self.guest_base.is_multiple_of(FIRMWARE_PAGE)
                || self.guest_base.checked_add(self.memory_size).is_none()
            {
                return Err(Error::Memory);
            }
            let rom_pages = u64::from(layout.rx_pages) + u64::from(layout.ro_pages);
            // Disk/network backends write guest RAM outside the CPU page table.
            // This static firmware ABI only exposes console, interrupt controller and RTC.
            if layout.rx_pages == 0
                || self.image.kernel_size == 0
                || rom_pages > self.image.kernel_memory_size.div_ceil(FIRMWARE_PAGE)
                || rom_pages * FIRMWARE_PAGE > self.memory_size - dtb - FIRMWARE_PAGE
                || self.image.kernel_memory_size < self.image.kernel_size
                || self.image.kernel_memory_size > self.memory_size - dtb
                || self.image.initrd_size != 0
                || self.disk.is_some()
                || self.writable
                || self.net_uplink
                || self.net_peer.is_some()
                || self.vcpus != 1
            {
                return Err(Error::Image);
            }
            return Ok(());
        }
        if !(MIN_RAM_BYTES..=MAX_RAM_BYTES).contains(&self.memory_size)
            || !self.guest_base.is_multiple_of(ALIGN)
            || !self.memory_size.is_multiple_of(ALIGN)
            || self.guest_base.checked_add(self.memory_size).is_none()
        {
            return Err(Error::Memory);
        }
        if self.image.kernel_memory_size < self.image.kernel_size
            || self.image.initrd_size > MAX_INITRD_BYTES
            || self.image.initrd_size > self.memory_size - KERNEL_OFFSET - ALIGN
            || KERNEL_OFFSET
                .checked_add(self.image.kernel_memory_size)
                .is_none_or(|end| end > self.initrd_offset())
        {
            return Err(Error::Image);
        }
        Ok(())
    }
    // Keep the small-image ABI addresses. Larger archives grow downward,
    // aligned to 2 MiB, leaving the final 2 MiB for DTB/firmware metadata.
    // Call after image/RAM bounds validation, not on arbitrary external data.
    pub fn initrd_offset(&self) -> u64 {
        if self.firmware.is_some() {
            return 0;
        } // Static firmware has no initrd.
        let archive = (self.image.initrd_size + ALIGN - 1) & !(ALIGN - 1);
        self.memory_size - (32 * 1024 * 1024).max(archive + ALIGN)
    }
    pub fn guest_dtb_offset(&self) -> u64 {
        self.memory_size
            - if self.firmware.is_some() {
                MAX_GUEST_DTB as u64
            } else {
                ALIGN
            }
    }
    /// Validate against real host topology at the input/board boundary, not on
    /// dispatch. Repeated owners are intentional: vCPUs may overcommit a hart.
    pub fn validate_placement(&self, host_cpus: u8) -> Result<(), Error> {
        if !(1..=MAX_GUEST_VCPUS).contains(&self.vcpus) {
            return Err(Error::Vcpus);
        }
        if !(1..=MAX_HOST_CPUS).contains(&host_cpus) {
            return Err(Error::Affinity);
        }
        if self.cpu_affinity == CpuAffinity::Workers && host_cpus < 2 {
            return Err(Error::Affinity);
        }
        if let CpuAffinity::Physical(owners) = self.cpu_affinity {
            for (index, hart) in owners.into_iter().enumerate() {
                if (index < usize::from(self.vcpus) && hart >= host_cpus)
                    || (index >= usize::from(self.vcpus) && hart != 0)
                {
                    return Err(Error::Affinity);
                }
            }
        }
        Ok(())
    }
    /// Prevalidated, immutable epoch placement. Guest hart IDs and physical
    /// owners are separate namespaces, including when the boot hart is not 0.
    pub fn owner(&self, slot: usize, index: usize, boot: usize, host_cpus: usize) -> usize {
        match self.cpu_affinity {
            CpuAffinity::Automatic => (boot + slot + index) % host_cpus,
            CpuAffinity::Physical(owners) => usize::from(owners[index]),
            CpuAffinity::Workers => (boot + 1 + (slot + index) % (host_cpus - 1)) % host_cpus,
        }
    }
    /// All native owners of this slot under the same prevalidated placement used by dispatch.
    pub fn owner_mask(&self, slot: usize, boot: usize, host_cpus: usize) -> usize {
        (0..usize::from(self.vcpus)).fold(0, |mask, index| {
            mask | (1usize << self.owner(slot, index, boot, host_cpus))
        })
    }
}
