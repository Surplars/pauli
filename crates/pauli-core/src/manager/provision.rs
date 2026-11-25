//! Runtime domains from immutable boot templates. Only Linux children can lease volumes.
//! Ownership/Lock Ordering: sole management writer; a reserved slot is invisible to workers.
//! Invariants: new RAM/arena, never a peer's live bytes; destroy is not a synonym for stop.
use super::leases::LeaseHandle;
use super::*;
use crate::board::VmResources;
use crate::memory::pool::DomainPool;
use pauli_config::{MAX_GUEST_DTB, VmConfig};
use pauli_control::{ChildSpec, DiskSlot, VmId};
const _: () = assert!(DiskSlot::COUNT == pauli_config::MAX_DISKS);

/// A clone never inherits an exclusive hart, disk lease, network peer or native uplink.
pub(super) fn cloneable(vm: &VmConfig) -> bool {
    !vm.exclusive_cpus && vm.disk.is_none() && vm.net_peer.is_none() && !vm.net_uplink
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
        let info = board.info();
        let mut config = shape::config(
            &source.config,
            spec,
            info.boot_cpu,
            info.host_cpus,
            self.exclusive_owners,
        )?;
        let reshape = config.memory_size != source.config.memory_size
            || config.vcpus != source.config.vcpus
            || spec.disk.is_some();
        // Firmware's linked load addresses and W^X page partition are fixed.
        // Only discovery for an explicitly omitted optional device may change.
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
        let sibling = match config.net_lease {
            Some(lease) => self.sibling_channel(lease, sponsor)?,
            None => None,
        };
        self.admit(sponsor, &config)?;
        let (slot, reserved) = self.index.reserve(self.boot_count)?;
        let owners =
            config.owner_mask_excluding(slot, info.boot_cpu, info.host_cpus, self.exclusive_owners);
        let target_sstc = config.firmware.is_none() && A::sstc_available_for_owners(owners);
        let source_sstc = source.guest_sstc();
        let discovery_changed = reshape
            || config.control_events != source.config.control_events
            || config.net_lease.is_some();
        let mut change =
            (discovery_changed || source_sstc != target_sstc).then_some(shape::DtbChange {
                source_sstc,
                target_sstc,
                optional: !discovery_changed && !source_sstc,
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
            pool,
            sibling,
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
        pool: &'static DomainPool,
        sibling: Option<LeaseHandle>,
    ) -> Result<Vm<A>> {
        config.host_base = resources.ram.host_range().base() as u64;
        if let Some((source, change)) = change {
            shape::rebuild::<A, B>(board, source, &mut config, &mut resources, change)?;
        }
        let epoch = self.next_epoch()?;
        let lease = match (config.net_lease, sibling) {
            (Some(_), None) => Some(LeaseHandle::new(pool, epoch)?),
            (_, handle) => handle,
        };
        Vm::build(
            board,
            self.config,
            config,
            slot,
            self.exclusive_owners,
            epoch,
            self.links,
            A::Timer::now(),
            resources,
            lease,
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
        // A rejected destroy must leave a running receiver visible to peer notifications.
        // Keep the final retirement checks under the cell's exclusive removal below.
        if !self.vm(slot)?.lifecycle.joined() {
            return Err(HvError::Busy);
        }
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
