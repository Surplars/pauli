//! Inactive VM construction; resolve physical resources once at the board boundary.
//! Ownership/Lock Ordering: management owns the private assignment before publication.
//! Invariants: no half-built VM is runnable; all image/range checks precede mutation.
use super::devices::DeviceLinks;
use super::*;
use crate::{
    HvError, Result,
    arch::{ArchVcpu, Stage2Space},
    board::{Board, Payload, VmResources},
};
use pauli_hal::sync::SpinLock;
impl<A: HvArch> Vm<A> {
    pub(crate) fn create<B: Board>(
        board: &B,
        boot: &pauli_config::BootConfig,
        config: VmConfig,
        slot: usize,
        epoch: u32,
        links: &'static DeviceLinks,
        now: u64,
    ) -> Result<Self> {
        config
            .validate_placement(board.info().host_cpus as u8)
            .map_err(|_| HvError::InvalidConfig)?;
        let resources = board.take_resources(slot, &config)?;
        Self::build(board, boot, config, slot, epoch, links, now, resources)
    }
    pub(crate) fn build<B: Board>(
        board: &B,
        boot: &pauli_config::BootConfig,
        config: VmConfig,
        slot: usize,
        epoch: u32,
        links: &'static DeviceLinks,
        now: u64,
        mut resources: VmResources,
    ) -> Result<Self> {
        let info = board.info();
        let quantum =
            u64::try_from(info.timebase_hz as u128 * config.quantum_us as u128 / 1_000_000)
                .ok()
                .filter(|ticks| *ticks != 0)
                .ok_or(HvError::InvalidConfig)?;
        config
            .validate_placement(info.host_cpus as u8)
            .map_err(|_| HvError::InvalidConfig)?;
        let payload = board.payload();
        let sstc =
            payload != Payload::Embedded && A::guest_sstc(resources.images.dtb, config.vcpus)?;
        if sstc
            && (payload != Payload::Linux
                || config.firmware.is_some()
                || !A::sstc_available_for_owners(config.owner_mask(
                    slot,
                    info.boot_cpu,
                    info.host_cpus,
                )))
        {
            return Err(HvError::Unsupported);
        }
        let image_boot = image::boot(&config, payload, info.timebase_hz, sstc)?;
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
            let owner = config.owner(slot, index, info.boot_cpu, info.host_cpus);
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
