//! Board loader policy: checked host-only firmware, manifest, image and guest ownership.
//! Ownership/Lock Ordering: one boot owner reserves fixed intervals before DMA allocation.
//! Invariants: no source or DTB pointer is formed until it is confined to retained RAM.
use crate::fdt::{Topology, parsed};
use pauli_config::{BootConfig, MAX_BOOT_VMS, MAX_GUEST_DTB, VmConfig, WIRE_SIZE, loader::*};
use pauli_core::{
    HvError, Result,
    board::{BootImages, VmResources},
    memory::{Arena, GuestRam, pool::TABLE_ARENA_BYTES},
};
use pauli_hal::{addr::PAGE_SIZE, fdt::Fdt, mem::PhysRange};
pub type Pool = pauli_core::memory::pool::FramePool;
pub struct Loader {
    pub config: BootConfig,
    pub pool: Pool,
    template: PhysRange,
    dtbs: PhysRange,
}

impl Loader {
    pub fn open(
        fdt: Fdt<'_>,
        ram: PhysRange,
        kernel_end: usize,
        firmware: PhysRange,
    ) -> Result<Self> {
        let mut pool = Pool::new(ram.base(), ram.size()).map_err(|_| HvError::InvalidMemory)?;
        let host = range(
            ram.base(),
            kernel_end
                .checked_sub(ram.base())
                .ok_or(HvError::InvalidMemory)?,
        )?;
        reserve(&mut pool, host)?;
        reserve(&mut pool, firmware)?;
        firmware_reservations(fdt, &mut pool)?;
        let base = ram.base() as u64;
        let (dtb_offset, config_offset) = metadata_offsets(ram.size() as u64);
        let manifest = range(
            chosen(fdt, CHOSEN_BOOT_CONFIG, base + config_offset)?,
            WIRE_SIZE,
        )?;
        free(&pool, manifest)?;
        // SAFETY: launcher/firmware retains the checked, currently unassigned RAM interval.
        let wire = unsafe { retained(manifest) };
        let config = BootConfig::decode(wire.try_into().map_err(|_| HvError::InvalidConfig)?)
            .map_err(|_| HvError::InvalidConfig)?;
        let template = range(config.template_base as usize, config.template_size as usize)?;
        let dtbs = range(
            chosen(fdt, CHOSEN_GUEST_DTBS, base + dtb_offset)?,
            config.dtb_size as usize,
        )?;
        free(&pool, template)?;
        free(&pool, dtbs)?;
        if manifest.overlaps(dtbs) {
            return Err(HvError::InvalidImage);
        }
        reserve(&mut pool, template)?;
        for metadata in [manifest, dtbs] {
            if !template.contains(metadata) {
                reserve(&mut pool, metadata)?;
            }
        }
        for vm in config.vms.iter().flatten() {
            let sources = [
                (vm.image.kernel_source, vm.image.kernel_size),
                (vm.image.initrd_source, vm.image.initrd_size),
            ];
            for (address, bytes) in sources {
                if bytes == 0 {
                    continue;
                }
                let image = range(address as usize, bytes as usize)?;
                if image.overlaps(manifest) || image.overlaps(dtbs) {
                    return Err(HvError::InvalidImage);
                }
            }
            reserve(
                &mut pool,
                range(vm.host_base as usize, vm.memory_size as usize)?,
            )?;
        }
        Ok(Self {
            config,
            pool,
            template,
            dtbs,
        })
    }
    pub fn resources(
        &mut self,
        topology: &Topology,
    ) -> Result<[Option<VmResources>; MAX_BOOT_VMS]> {
        self.stage_timers(topology)?;
        let mut resources = [const { None }; MAX_BOOT_VMS];
        for (index, config) in self.config.vms.into_iter().enumerate() {
            if let Some(vm) = config {
                resources[index] = Some(self.resources_for(vm, topology.hz)?);
            }
        }
        Ok(resources)
    }
    /// The launcher cannot know firmware's Sstc grants. Before any secondary is
    /// started, only guests wholly assigned to probed harts may gain Sstc.
    fn stage_timers(&mut self, topology: &Topology) -> Result<()> {
        let mut upgrade = [false; MAX_BOOT_VMS];
        for (index, vm) in self.config.vms.iter().flatten().enumerate() {
            if vm.firmware.is_none() && vm.validate_placement(topology.count as u8).is_ok() {
                let owners = vm.owner_mask(index, topology.boot, topology.count);
                upgrade[index] = pauli_arch_riscv::sstc_available_for_owners(owners);
            }
        }
        if !upgrade.contains(&true) {
            return Ok(());
        }
        let hz = u32::try_from(topology.hz).map_err(|_| HvError::InvalidConfig)?;
        // Keep manifest offsets and lengths consistent with the staged bundle.
        // One retained HS allocation also keeps the original loader byte ranges immutable.
        let trees = match self.take_bytes(self.config.count() * MAX_GUEST_DTB, PAGE_SIZE) {
            Ok(trees) => trees,
            // The existing canonical baseline still boots with SBI TIME.
            Err(HvError::Exhausted) => return Ok(()),
            Err(error) => return Err(error),
        };
        for (index, mut vm) in self.config.vms.into_iter().flatten().enumerate() {
            let original = self.guest_tree(&vm)?;
            let offset = index * MAX_GUEST_DTB;
            let slot = &mut trees[offset..offset + MAX_GUEST_DTB];
            let extended = if upgrade[index] {
                qemu_virt_riscv::guest_tree::enable_sstc(&vm, original, hz, slot)
            } else {
                None
            };
            let size = extended.unwrap_or_else(|| {
                slot[..original.len()].copy_from_slice(original);
                original.len()
            });
            vm.dtb_offset = offset as u64;
            vm.dtb_size = size as u64;
            self.config.vms[index] = Some(vm);
        }
        self.dtbs = range(trees.as_ptr() as usize, trees.len())?;
        self.config.dtb_size = trees.len() as u64;
        Ok(())
    }
    fn guest_tree(&self, vm: &VmConfig) -> Result<&'static [u8]> {
        self.source(
            self.dtbs,
            self.dtbs.base() + vm.dtb_offset as usize,
            vm.dtb_size as usize,
        )
    }
    fn resources_for(&mut self, vm: VmConfig, hz: u64) -> Result<VmResources> {
        let image = vm.image;
        let kernel = self.source(
            self.template,
            image.kernel_source as usize,
            image.kernel_size as usize,
        )?;
        let initrd = self.source(
            self.template,
            image.initrd_source as usize,
            image.initrd_size as usize,
        )?;
        let dtb = self.guest_tree(&vm)?;
        let cpus = parsed(parsed(Fdt::new(dtb))?.find("/cpus"))?.ok_or(HvError::InvalidImage)?;
        if parsed(cpus.u32("timebase-frequency"))?.map(u64::from) != Some(hz) {
            return Err(HvError::InvalidImage);
        }
        let bytes = TABLE_ARENA_BYTES
            + pauli_core::vm::Vm::<pauli_arch_riscv::interface::RiscvHvArch>::vcpu_arena_bytes(
                vm.vcpus,
            )?
            + if vm.disk.is_some() {
                pauli_core::backends::block::BUFFER_BYTES
            } else {
                0
            };
        let storage = self.take_bytes(bytes, PAGE_SIZE)?;
        // SAFETY: open reserved this VM's unique RAM before any arena allocation;
        // parsed/retained loader sources are disjoint from its backing.
        let ram = unsafe {
            GuestRam::new(
                range(vm.host_base as usize, vm.memory_size as usize)?,
                vm.guest_base as usize,
            )
        }?;
        Ok(VmResources {
            ram,
            arena: Arena::new(storage),
            images: BootImages {
                kernel,
                initrd,
                dtb,
            },
        })
    }
    fn source(&self, owner: PhysRange, base: usize, size: usize) -> Result<&'static [u8]> {
        if size == 0 {
            return Ok(&[]);
        }
        let part = range(base, size)?;
        if !owner.contains(part) {
            return Err(HvError::InvalidImage);
        }
        // SAFETY: the validated subrange belongs to an immutable retained loader reservation.
        Ok(unsafe { retained(part) })
    }
    pub fn take_bytes(&mut self, bytes: usize, alignment: usize) -> Result<&'static mut [u8]> {
        let region = self
            .pool
            .allocate(bytes, alignment)
            .map_err(|_| HvError::Exhausted)?;
        // SAFETY: freshly reserved, exclusive host allocation, never VM backing.
        // Initialize firmware-provided RAM before creating a Rust byte slice.
        unsafe {
            let ptr = region.base() as *mut u8;
            ptr.write_bytes(0, region.size());
            Ok(core::slice::from_raw_parts_mut(ptr, region.size()))
        }
    }
}
fn chosen(fdt: Fdt<'_>, name: &str, default: u64) -> Result<usize> {
    let Some(node) = parsed(fdt.find("/chosen"))? else {
        return Ok(default as usize);
    };
    let Some(raw) = parsed(node.property(name))? else {
        return Ok(default as usize);
    };
    let bytes: [u8; 8] = raw.try_into().map_err(|_| HvError::InvalidConfig)?;
    Ok(u64::from_be_bytes(bytes) as usize)
}
fn firmware_reservations(fdt: Fdt<'_>, pool: &mut Pool) -> Result<()> {
    parsed(fdt.for_each_reservation(|base, size| {
        let reserved =
            range(base as usize, size as usize).map_err(|_| pauli_hal::fdt::Error::Reservation)?;
        protect(pool, reserved).map_err(|_| pauli_hal::fdt::Error::Reservation)
    }))?;
    let mut inside = false;
    for node in fdt.nodes() {
        let node = parsed(node)?;
        if node.depth() == 1 {
            inside = node.name() == "reserved-memory";
        }
        if inside && node.depth() == 2 {
            if let Some((base, size)) = parsed(node.reg())? {
                protect(pool, range(base as usize, size as usize)?)?;
            }
        }
    }
    Ok(())
}
fn protect(pool: &mut Pool, reserved: PhysRange) -> Result<()> {
    if !pool.ram().overlaps(reserved) || pool.reservations().any(|other| other.contains(reserved)) {
        return Ok(());
    }
    reserve(pool, reserved)
}
fn free(pool: &Pool, region: PhysRange) -> Result<()> {
    if !pool.ram().contains(region) || pool.reservations().any(|used| used.overlaps(region)) {
        Err(HvError::InvalidMemory)
    } else {
        Ok(())
    }
}
fn reserve(pool: &mut Pool, region: PhysRange) -> Result<()> {
    pool.reserve_range(region.base(), region.size())
        .map(|_| ())
        .map_err(|_| HvError::InvalidMemory)
}
fn range(base: usize, size: usize) -> Result<PhysRange> {
    PhysRange::new(base, size).map_err(|_| HvError::InvalidMemory)
}
unsafe fn retained(region: PhysRange) -> &'static [u8] {
    // SAFETY: caller proved the immutable, propertly initialized physical loader reservation.
    unsafe { core::slice::from_raw_parts(region.base() as *const u8, region.size()) }
}
