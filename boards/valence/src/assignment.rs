//! Transfer private per-slot resources once; inactive slots never borrow an arena.
//! The boot hart owns both arenas before publication. No slot can take a peer's
//! backing, DTB or metadata by retrying a provisioning request.
use core::mem::MaybeUninit;
use pauli_config::BootConfig;
use pauli_core::{
    HvError, Result,
    board::{BootImages, VmResources},
    memory::{
        Arena, GuestRam,
        pool::{DomainPool, FramePool},
    },
};
use pauli_hal::mem::PhysRange;

/// # Safety
/// The complete range must be an exclusive, writable, identity-mapped loan
/// retained for the lifetime of the pool, disjoint from all other board owners.
pub(super) unsafe fn bind_pool(range: Option<PhysRange>) -> Result<Option<DomainPool>> {
    let Some(range) = range else { return Ok(None) };
    let frames = FramePool::new(range.base(), range.size()).map_err(|_| HvError::InvalidMemory)?;
    // SAFETY: the boot allocator contains exactly the dedicated no-map loan.
    Ok(Some(unsafe { DomainPool::from_boot(frames) }))
}

pub(super) type Images = [Option<BootImages>; 2];
// A 64 KiB stride preserves the first arena's linker-provided 16 KiB root
// alignment for the service, including after either VM's independent restart.
pub(super) type Arenas = [MaybeUninit<[u8; 64 * 1024]>; 2];
pub(super) struct Slots([Option<VmResources>; 2]);
impl Slots {
    /// # Safety
    /// All configured RAM ranges are exclusive identity-mapped grants, disjoint
    /// from each other, the template slices and these private boot-time arenas.
    pub(super) unsafe fn bind(
        boot: &BootConfig,
        images: Images,
        arenas: &'static mut Arenas,
    ) -> Result<Self> {
        let mut slots = [None, None];
        for (slot, ((dst, images), arena)) in slots.iter_mut().zip(images).zip(arenas).enumerate() {
            match (boot.vms[slot], images) {
                (None, None) => continue,
                (Some(vm), Some(images)) => {
                    let range = PhysRange::new(vm.host_base as usize, vm.memory_size as usize)
                        .map_err(|_| HvError::InvalidMemory)?;
                    // SAFETY: this slot owns the designated guest RAM and its own
                    // NOLOAD arena; initialize the latter before lending a byte slice.
                    let (ram, storage) = unsafe {
                        arena.as_mut_ptr().write_bytes(0, 1);
                        (
                            GuestRam::new(range, vm.guest_base as usize)?,
                            arena.assume_init_mut(),
                        )
                    };
                    *dst = Some(VmResources {
                        ram,
                        arena: Arena::new(storage),
                        images,
                    });
                }
                _ => return Err(HvError::InvalidConfig),
            }
        }
        Ok(Self(slots))
    }
    pub(super) fn take(&mut self, slot: usize) -> Result<VmResources> {
        self.0
            .get_mut(slot)
            .and_then(Option::take)
            .ok_or(HvError::InvalidMemory)
    }
}
