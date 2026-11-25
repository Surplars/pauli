//! One authoritative bounded VM collection and private peer link.
//! Ownership/Lock Ordering: one management writer; dispatch pins each removable slot.
//! Invariants: epoch IDs never wrap/reuse; no global RTOS VM table exists.
use crate::{
    HvError, Result,
    arch::{ArchTimer, HvArch, MAX_CPUS},
    board::{Board, BoardInfo, Payload},
    placement::{Placement, Published},
    vm::{Vm, devices::DeviceLinks},
};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use pauli_config::{BootConfig, MAX_BOOT_VMS as BOOT_SLOTS};
use pauli_devices::net_link::Link;
use pauli_hal::sync::{Once, SpinLock};
mod admission;
mod cell;
mod directory;
pub use admission::Budget;
mod provision;
mod shape;
use cell::{Cell, View};
use directory::{Directory, Group};

pub(crate) struct VmStore<A: HvArch> {
    claimed: AtomicBool,
    boot: [Once<Vm<A>>; BOOT_SLOTS],
    first: Group<Vm<A>>,
    boot_lanes: [Published; MAX_CPUS],
    links: DeviceLinks,
}
impl<A: HvArch> VmStore<A> {
    pub(crate) const fn new() -> Self {
        Self {
            claimed: AtomicBool::new(false),
            boot: [const { Once::new() }; BOOT_SLOTS],
            first: Group::new(),
            boot_lanes: [const { Published::empty() }; MAX_CPUS],
            links: DeviceLinks::new(),
        }
    }
}
pub struct VmManager<A: HvArch> {
    // Initialized once at boot. Workers scan this immutable view without
    // atomically probing each unoccupied backing slot on every slice.
    slots: [Option<&'static Vm<A>>; BOOT_SLOTS],
    boot_count: usize,
    index: Directory<Vm<A>>,
    links: &'static DeviceLinks,
    pub config: &'static BootConfig,
    pub errors: [Option<HvError>; BOOT_SLOTS],
    epoch: AtomicU32,
}
impl<A: HvArch> VmManager<A> {
    pub(crate) fn create<B: Board>(board: &'static B, store: &'static VmStore<A>) -> Result<Self> {
        let config = board.load_boot_config()?;
        match board.payload() {
            Payload::Embedded => crate::vm::validate_embedded(config)?,
            _ => config.validate().map_err(|_| HvError::InvalidConfig)?,
        }
        validate_info(board.info())?;
        let boot_count = config.count(); // Validated records form a contiguous prefix.
        // A manifest can contain Linux or fixed-layout firmware. The smallest
        // eligible child bounds the ID space, not the number of boot entries.
        let pool = board
            .domain_pool()
            .filter(|_| config.management && board.payload() == Payload::Linux);
        let mut spare = 0;
        if let Some(pool) = pool {
            let metadata = Vm::<A>::vcpu_arena_bytes(1)?;
            for vm in config
                .vms
                .iter()
                .flatten()
                .filter(|vm| provision::cloneable(vm))
            {
                spare = spare.max(pool.capacity::<Vm<A>>(vm, metadata)?);
            }
        }
        // Wire IDs are u32 and RPC channels also encode a vCPU index in usize.
        let capacity = (boot_count + spare)
            .min(u32::MAX as usize)
            .min(usize::MAX / crate::placement::STRIDE);
        if store.claimed.swap(true, Ordering::AcqRel) {
            return Err(HvError::Busy);
        }
        let index = Directory::new(
            &store.first,
            &store.boot_lanes[..board.info().host_cpus],
            pool,
            capacity,
        )?;
        let mut slots = [None; BOOT_SLOTS];
        let mut errors = [None; BOOT_SLOTS];
        for (slot, vm) in config.vms.iter().enumerate() {
            let Some(vm) = vm else {
                continue;
            };
            let created = Vm::create(
                board,
                config,
                *vm,
                slot,
                slot as u32 + 1,
                &store.links,
                A::Timer::now(),
            );
            match created {
                Ok(vm) => {
                    index.set(slot, owner_masks(&vm), true);
                    store.boot[slot].set(vm).map_err(|_| HvError::Busy)?;
                    slots[slot] = store.boot[slot].get();
                }
                Err(error) => errors[slot] = Some(error),
            }
        }
        Ok(Self {
            slots,
            boot_count,
            index,
            links: &store.links,
            config,
            errors,
            epoch: AtomicU32::new(BOOT_SLOTS as u32 + 1),
        })
    }
    /// Highest addressable guest ID, not a promise that every requested shape fits.
    pub fn capacity(&self) -> usize {
        self.index.limit()
    }
    pub(crate) fn vms(&self) -> impl Iterator<Item = View<'_, Vm<A>>> {
        self.index.live().filter_map(|slot| self.available(slot))
    }
    pub(crate) fn placement_groups(&self) -> usize {
        self.index.groups()
    }
    pub(crate) fn has_assignments(&self, owner: usize) -> bool {
        self.index.has_assignments(owner)
    }
    pub(crate) fn placed_on(&self, group: usize, owner: usize) -> Placement {
        self.index.placement(group, owner)
    }
    pub fn link(&self) -> &SpinLock<Link> {
        &self.links.peer
    }
    pub(crate) fn available(&self, slot: usize) -> Option<View<'_, Vm<A>>> {
        if slot < self.boot_count {
            self.slots[slot].map(View::Fixed)
        } else {
            self.child_cell(slot)?.read()
        }
    }
    fn child_cell(&self, slot: usize) -> Option<&Cell<Vm<A>>> {
        self.index.cell(slot)
    }
    pub(crate) fn vm(&self, slot: usize) -> Result<View<'_, Vm<A>>> {
        self.available(slot).ok_or(
            self.errors
                .get(slot)
                .copied()
                .flatten()
                .unwrap_or(HvError::NotFound),
        )
    }
    pub fn find(&self, name: &str) -> Result<usize> {
        if let Ok(id) = name.parse::<usize>() {
            let slot = id.checked_sub(1).ok_or(HvError::NotFound)?;
            return self.vm(slot).map(|_| slot);
        }
        self.vms()
            .find(|vm| vm.config.name() == name)
            .map(|vm| vm.slot)
            .ok_or(HvError::NotFound)
    }
    pub fn start<B: Board>(&self, board: &B, slot: usize) -> Result<()> {
        let vm = self.vm(slot)?;
        if !vm.lifecycle.joined() {
            return Err(HvError::Busy);
        }
        let epoch = self.next_epoch().inspect_err(|&error| {
            // No new epoch exists to attribute this failure to.
            vm.activation_failed(vm.epoch(), error);
        })?;
        vm.start(board, epoch, self.link(), A::Timer::now())
    }
    fn next_epoch(&self) -> Result<u32> {
        self.epoch
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                old.checked_add(1)
            })
            .map_err(|_| HvError::Exhausted)
    }
    pub fn service<B: Board>(&self, board: &B) {
        let now = A::Timer::now();
        board.service_devices(now);
        for vm in self.vms() {
            if vm.lifecycle.stopping() {
                match vm.try_join(now, self.link()) {
                    Ok(true) if vm.lifecycle.reset_requested() && !vm.config.stop_on_reset => {
                        let _ = self.start(board, vm.slot);
                    }
                    // The CPU side has joined, but I/O is not reusable. Keep
                    // the VM stopped at the barrier and surface the error once.
                    Err(error) => vm.fail_join(error),
                    _ => {}
                }
            } else if vm.devices.service(A::Timer::now, self.link()).is_err() {
                vm.request_stop(true, false);
            }
            vm.deliver_wakes();
        }
    }
}
fn owner_masks<A: HvArch>(vm: &Vm<A>) -> [usize; MAX_CPUS] {
    let mut masks = [0; MAX_CPUS];
    for cpu in vm.cpus() {
        masks[cpu.owner] |= 1 << cpu.index;
    }
    masks
}
fn validate_info(info: BoardInfo) -> Result<()> {
    if info.host_cpus == 0
        || info.host_cpus > crate::arch::MAX_CPUS
        || info.boot_cpu >= info.host_cpus
        || info.timebase_hz == 0
        || info.ram_base.checked_add(info.ram_size).is_none()
        || info.kernel_end < info.ram_base
        || info.kernel_end >= info.ram_base + info.ram_size
    {
        return Err(HvError::InvalidConfig);
    }
    Ok(())
}
