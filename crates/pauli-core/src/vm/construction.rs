//! Inactive VM construction; resolve physical resources once at the board boundary.
//! Ownership/Lock Ordering: management owns the private assignment before publication.
//! Invariants: no half-built VM is runnable; all image/range checks precede mutation.
use super::devices::DeviceLinks;
use super::*;
use crate::{
    HvError, Result,
    arch::{ArchVcpu, Stage2Space},
    board::{Board, Payload, VmResources},
    manager::leases::LeaseHandle,
};
use pauli_hal::sync::SpinLock;
impl<A: HvArch> Vm<A> {
    pub(crate) fn create<B: Board>(
        board: &B,
        boot: &pauli_config::BootConfig,
        config: VmConfig,
        slot: usize,
        exclusive_owners: u64,
        epoch: u32,
        links: &'static DeviceLinks,
        now: u64,
    ) -> Result<Self> {
        // The manager validated all placements before taking any board resources.
        let resources = board.take_resources(slot, &config)?;
        Self::build(
            board,
            boot,
            config,
            slot,
            exclusive_owners,
            epoch,
            links,
            now,
            resources,
            None,
        )
    }
    pub(crate) fn build<B: Board>(
        board: &B,
        boot: &pauli_config::BootConfig,
        config: VmConfig,
        slot: usize,
        exclusive_owners: u64,
        epoch: u32,
        links: &'static DeviceLinks,
        now: u64,
        mut resources: VmResources,
        lease: Option<LeaseHandle>,
    ) -> Result<Self> {
        let info = board.info();
        let quantum =
            u64::try_from(info.timebase_hz as u128 * config.quantum_us as u128 / 1_000_000)
                .ok()
                .filter(|ticks| *ticks != 0)
                .ok_or(HvError::InvalidConfig)?;
        let payload = board.payload();
        if config.control_events
            && (payload == Payload::Embedded || !cfg!(feature = "guest-control"))
        {
            return Err(HvError::Unsupported);
        }
        if payload != Payload::Embedded {
            A::validate_control_events(resources.images.dtb, &config, board.layout())?;
        }
        let sstc =
            payload != Payload::Embedded && A::guest_sstc(resources.images.dtb, config.vcpus)?;
        if sstc
            && (payload != Payload::Linux
                || config.firmware.is_some()
                || !A::sstc_available_for_owners(config.owner_mask_excluding(
                    slot,
                    info.boot_cpu,
                    info.host_cpus,
                    exclusive_owners,
                )))
        {
            return Err(HvError::Unsupported);
        }
        let image_boot = image::boot(&config, payload, info.timebase_hz, sstc)?;
        if let Payload::Firmware(layout) = image_boot.payload {
            resources.ram = resources.ram.for_firmware(layout)?;
        }
        let image = image::prepare(
            &config,
            resources.images,
            &resources.ram,
            image_boot.payload,
        )?;
        // SAFETY: board gave one inactive, exclusively assigned RAM and private table arena.
        let space =
            unsafe { A::Space::create(&resources.ram, &mut resources.arena, image_boot.payload) }?;
        // SAFETY: no vCPU or device producer has been created or published yet.
        unsafe { resources.ram.restore_retired(&image) }?;
        A::publish_code();
        let cpus = ArenaSlice::build(&mut resources.arena, usize::from(config.vcpus), |index| {
            let cpu = A::Vcpu::create(&space, index as u8, epoch, &config, image_boot)?;
            let owner = config.owner_excluding(
                slot,
                index,
                info.boot_cpu,
                info.host_cpus,
                exclusive_owners,
            );
            Ok(VcpuSlot::new(cpu, epoch, index as u8, owner))
        })?;
        let devices = VmDevices::create(
            board,
            boot,
            &config,
            slot,
            epoch,
            resources.ram,
            &mut resources.arena,
            links,
            now,
            lease,
        )?;
        Ok(Self {
            config,
            slot,
            space,
            cpus,
            devices,
            quantum,
            lifecycle: lifecycle::Lifecycle::new(),
            notice: lifecycle::Notice::new(),
            image,
            allocation: None,
            sponsor: None,
            quota: SpinLock::new(None),
            boot: image_boot,
            epoch: AtomicU32::new(epoch),
            started: AtomicBool::new(false),
            diagnostics: diagnostics::Diagnostics::new(),
            terminal: SpinLock::new(None),
        })
    }
}
