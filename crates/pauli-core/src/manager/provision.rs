//! Runtime domains from immutable boot templates. Only Linux children can lease volumes.
//! Ownership/Lock Ordering: sole management writer; a reserved slot is invisible to workers.
//! Invariants: new RAM/arena, never a peer's live bytes; destroy is not a synonym for stop.
use super::*;
use crate::board::VmResources;
use pauli_config::{MAX_GUEST_DTB, VmConfig};
use pauli_control::{ChildSpec, DiskSlot, VmId};
const _: () = assert!(DiskSlot::COUNT == pauli_config::MAX_DISKS);

/// A clone never inherits a disk lease, network peer or native uplink.
pub(super) fn cloneable(vm: &VmConfig) -> bool {
    vm.disk.is_none() && vm.net_peer.is_none() && !vm.net_uplink
}

impl<A: HvArch> VmManager<A> {
    pub(crate) fn provision<B: Board>(
        &self,
        board: &'static B,
        sponsor: Option<VmId>,
        template: usize,
        spec: ChildSpec,
    ) -> Result<usize> {
        let source = self
            .slots
            .get(template)
            .copied()
            .flatten()
            .ok_or(HvError::NotFound)?;
        if !self.config.management
            || board.payload() != Payload::Linux
            || !cloneable(&source.config)
        {
            return Err(HvError::Unsupported);
        }
        let mut config = shape::config(&source.config, spec, board.info().host_cpus as u8)?;
        let reshape = config.memory_size != source.config.memory_size
            || config.vcpus != source.config.vcpus
            || spec.disk.is_some();
        // Firmware's linked layout, immutable DTB and W^X page partition are
        // one contract. Reuse it unchanged; never feed it to the Linux DTB builder.
        if config.firmware.is_some() && reshape {
            return Err(HvError::Unsupported);
        }
        if let Some(disk) = spec.disk {
            self.assign_volume(&mut config, disk)?;
        }
        let name = config.name();
        // Failed boot slots and their names remain reserved, not available for adoption.
        if self.config.vms.iter().flatten().any(|vm| vm.name() == name) || self.find(name).is_ok() {
            return Err(HvError::Busy);
        }
        let pool = board.domain_pool().ok_or(HvError::Unsupported)?;
        self.admit(sponsor, &config)?;
        let (slot, reserved) = self.index.reserve(self.boot_count)?;
        let info = board.info();
        let owners = config.owner_mask(slot, info.boot_cpu, info.host_cpus);
        let target_sstc = config.firmware.is_none() && A::sstc_available_for_owners(owners);
        let source_sstc = source.guest_sstc();
        let mut change = (reshape || source_sstc != target_sstc).then_some(shape::DtbChange {
            source_sstc,
            target_sstc,
            optional: !reshape && !source_sstc,
        });
        let metadata = Vm::<A>::vcpu_arena_bytes(config.vcpus)?
            + if config.disk.is_some() {
                crate::backends::block::BUFFER_BYTES
            } else {
                0
            };
        let allocate =
            |extra| pool.allocate::<Vm<A>>(&config, source.image.sources(), metadata + extra);
        let (resources, storage, receipt) =
            match allocate(if change.is_some() { MAX_GUEST_DTB } else { 0 }) {
                Err(HvError::Exhausted) if change.is_some_and(|plan| plan.optional) => {
                    // A clone that fits with its original, safe discovery image is admissible.
                    change = None;
                    allocate(0)?
                }
                result => result?,
            };
        let result = self.build_child(
            board,
            config,
            change.map(|plan| (&source.config, plan)),
            slot,
            resources,
        );
        let mut vm = match result {
            Ok(vm) => vm,
            Err(error) => {
                // SAFETY: an unpublished frontend has never submitted I/O or run a vCPU.
                // Only driver-owned memory is DMA-visible; its event lives in the store.
                unsafe { receipt.release() };
                return Err(error);
            }
        };
        vm.allocation = Some(receipt);
        vm.sponsor = sponsor;
        let masks = owner_masks(&vm);
        reserved.publish(storage, vm); // Joined; start kicks its published owners.
        self.index.set(slot, masks, true);
        Ok(slot)
    }

    fn build_child<B: Board>(
        &self,
        board: &B,
        mut config: VmConfig,
        change: Option<(&VmConfig, shape::DtbChange)>,
        slot: usize,
        mut resources: VmResources,
    ) -> Result<Vm<A>> {
        config.host_base = resources.ram.host_range().base() as u64;
        if let Some((source, change)) = change {
            shape::rebuild::<A, B>(board, source, &mut config, &mut resources, change)?;
        }
        Vm::build(
            board,
            self.config,
            config,
            slot,
            self.next_epoch()?, // Rejected geometry or exhausted RAM never consumes an identity.
            self.links,
            A::Timer::now(),
            resources,
        )
    }

    /// The published VM is the lease, including while stopped or faulted. The
    /// single management writer cannot admit a successor until joined destruction.
    fn assign_volume(&self, vm: &mut VmConfig, slot: DiskSlot) -> Result<()> {
        let index = slot.index();
        let disk = self.config.disks[usize::from(index)].ok_or(HvError::NotFound)?;
        if !disk.managed {
            return Err(HvError::Permission);
        }
        if self.volume_owner(index).is_some() {
            return Err(HvError::Busy);
        }
        vm.disk = Some(index);
        vm.writable = disk.writable;
        Ok(())
    }
    pub(crate) fn volume_owner(&self, disk: u8) -> Option<usize> {
        self.vms()
            .find(|vm| vm.config.disk == Some(disk))
            .map(|vm| vm.slot)
    }
    pub(crate) fn destroy(&self, slot: usize, epoch: u32) -> Result<()> {
        if slot < self.boot_count {
            return Err(HvError::Permission);
        }
        let cell = self.child_cell(slot).ok_or(HvError::NotFound)?;
        let receipt = cell.remove(|vm| {
            if vm.epoch() != epoch
                || !vm.lifecycle.joined()
                || !vm.retired()
                || vm.join_error().is_some()
                || self
                    .vms()
                    .any(|child| child.sponsor == Some(VmId(slot as u32 + 1)))
            {
                return Err(HvError::Busy);
            }
            let receipt = vm.allocation.take().ok_or(HvError::InvalidState)?;
            self.index.set(slot, owner_masks(vm), false);
            Ok(receipt)
        })?;
        // SAFETY: joined CPU/TLB/I/O barrier and last reader proved; remove dropped
        // the VM, including its stage-2 tables, before returning the extent receipt.
        unsafe { receipt.release() };
        Ok(())
    }
}
