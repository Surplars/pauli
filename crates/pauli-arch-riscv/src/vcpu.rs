//! ISA-owned vCPU execution, without RTOS IDs, leases or a parallel VM state table.
//! Ownership/Lock Ordering: caller owns the off-CPU context; no lock spans entry.
//! Invariants: bounded physical deadline, checked decode, private IRQ levels and per-owner retirement.

use crate::{
    Error, GStagePageTable, VCpuContext, VmExit, bits,
    sbi::{self, SbiAction},
    timer::{PhysicalTimer, VirtualTimer},
};
pub trait GuestIo {
    fn mmio(&mut self, access: crate::mmio::MmioAccess) -> Option<usize>;
    fn csr(&mut self, _access: crate::aia::emulation::CsrAccess) -> Option<usize> {
        None
    }
}
pub struct RunOptions<'a> {
    pub space: &'a GStagePageTable,
    pub host_deadline: u64,
    pub io: &'a mut dyn GuestIo,
    pub physical: &'a mut PhysicalTimer,
    pub resident: Option<&'a mut crate::resident::Resident>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    Resume,
    Wait,
    Deferred,
    Shutdown,
    Fault,
}
#[derive(Debug)]
pub struct ManagedExit {
    pub raw: VmExit,
    pub disposition: Disposition,
    pub sbi: Option<SbiAction>,
    pub wfi: bool,
    pub mmio: bool,
    pub host_deadline_reached: bool,
}
pub struct VirtualCpu {
    epoch: u32,
    index: u8,
    smp: bool,
    ctx: VCpuContext,
    timer: VirtualTimer,
    external_pending: bool,
    resident_harts: usize,
    translations: crate::vmid::Statistics,
}
impl VirtualCpu {
    pub fn new(
        epoch: u32,
        index: u8,
        count: u8,
        mut ctx: VCpuContext,
        space: &GStagePageTable,
    ) -> Result<Self, Error> {
        if !crate::available() {
            return Err(Error::Unsupported);
        }
        if epoch == 0 || index >= count || count as usize > crate::MAX_VCPUS {
            return Err(Error::InvalidVcpu);
        }
        if ctx.pc & 1 != 0
            || ctx.vsatp != 0
            || !space.is_executable(pauli_hal::addr::PhysAddr::new(ctx.pc))
        {
            return Err(Error::InvalidImage);
        }
        ctx.hgatp = space.hgatp_value();
        ctx.tlb_epoch = epoch;
        ctx.tlb_vcpu = index;
        ctx.hvip = 0;
        ctx.vsip &= !(bits::SSIP | bits::STIP | bits::SEIP);
        if count > 1 {
            ctx.hideleg |= bits::VSSIP;
        }
        Ok(Self {
            epoch,
            index,
            smp: count > 1,
            ctx,
            timer: VirtualTimer::new(),
            external_pending: false,
            resident_harts: 0,
            translations: crate::vmid::Statistics::default(),
        })
    }
    pub fn id(&self) -> u32 {
        self.epoch
    }
    pub fn index(&self) -> u8 {
        self.index
    }
    pub fn context(&self) -> &VCpuContext {
        &self.ctx
    }
    pub fn timer(&self) -> VirtualTimer {
        self.timer
    }
    pub fn virtual_aia(&self) -> bool {
        self.ctx.virtual_aia
    }
    pub fn translations_retired(&self) -> bool {
        self.resident_harts == 0
    }
    pub fn resident_harts(&self) -> usize {
        self.resident_harts
    }
    pub fn translation_stats(&self) -> crate::vmid::Statistics {
        self.translations
    }
    pub fn retire_local_translations(&mut self) {
        if self.resident_harts != 0 {
            let bit = crate::vmid::retire(&mut self.ctx, self.resident_harts);
            self.resident_harts &= !bit;
            self.translations.retirements += u64::from(bit != 0);
        }
    }
    /// # Safety
    /// This physical owner has put any resident/file bank, owns this off-CPU
    /// context with IRQs masked, and retains its immutable backing through ACK.
    pub unsafe fn synchronize_translations(&mut self, batch: crate::remote::FenceBatch) {
        // SAFETY: matching physical owner and retained epoch/backing contract.
        let actual = unsafe { crate::vmid::synchronize(&self.ctx, batch) };
        self.translations.remote_syncs += 1;
        self.translations.remote_tlb += u64::from(actual.translation);
        self.translations.remote_icache += u64::from(actual.instruction);
    }
    pub fn set_external_line(&mut self, asserted: bool) {
        self.ctx.hideleg |= bits::VSEIP;
        self.external_pending = asserted || (self.ctx.guest_file != 0 && self.external_pending);
        self.refresh(crate::host::timer::read_time());
    }
    pub fn inject_software_interrupt(&mut self) {
        self.ctx.hideleg |= bits::VSSIP;
        self.ctx.hvip |= bits::VSSIP;
        self.ctx.vsip |= bits::SSIP;
    }
    pub fn clear_pending(&mut self) {
        self.timer = VirtualTimer::new();
        self.external_pending = false;
        self.ctx.hvip = 0;
        self.ctx.vsip &= !(bits::SSIP | bits::STIP | bits::SEIP);
    }
    pub fn wait_deadline(&self) -> Option<u64> {
        if self.ctx.vsie & bits::STIP != 0 {
            self.timer.deadline()
        } else {
            None
        }
    }
    pub fn poll_wakeup(&mut self) -> bool {
        self.refresh(crate::host::timer::read_time());
        self.enabled_pending()
    }
    pub fn finish_sbi(&mut self, error: isize, value: usize) -> Result<(), Error> {
        let next = self.ctx.pc.checked_add(4).ok_or(Error::InvalidImage)?;
        self.ctx.x[10] = error as usize;
        self.ctx.x[11] = value;
        self.ctx.pc = next;
        Ok(())
    }
    pub fn reset_secondary(
        &mut self,
        entry: usize,
        opaque: usize,
        space: &GStagePageTable,
    ) -> Result<(), Error> {
        if !self.translations_retired() {
            return Err(Error::Busy);
        }
        if entry & 1 != 0 || !space.is_executable(pauli_hal::addr::PhysAddr::new(entry)) {
            return Err(Error::InvalidImage);
        }
        let mut ctx = VCpuContext::new();
        ctx.pc = entry;
        ctx.x[10] = self.index as usize;
        ctx.x[11] = opaque;
        if self.ctx.guest_os {
            ctx.enable_guest_os();
        }
        ctx.fp_enabled = self.ctx.fp_enabled;
        ctx.virtual_aia = self.ctx.virtual_aia;
        if self.smp {
            ctx.hideleg |= bits::VSSIP;
        }
        ctx.hgatp = space.hgatp_value();
        ctx.tlb_epoch = self.epoch;
        ctx.tlb_vcpu = self.index;
        self.ctx = ctx;
        self.clear_pending();
        Ok(())
    }
    #[cfg(feature = "aia-guest-file")]
    pub(crate) fn bind_imsic(&mut self, file: u8, external: bool) {
        // Only the physical file-cache owner may call this on a provisioned vCPU.
        debug_assert!(self.ctx.virtual_aia);
        self.ctx.guest_file = file;
        self.external_pending = external;
        self.refresh(crate::host::timer::read_time());
    }
    fn refresh(&mut self, now: u64) {
        self.timer.refresh(now);
        let software = self.ctx.hvip & bits::VSSIP;
        let external = if self.external_pending && self.ctx.guest_file == 0 {
            bits::VSEIP
        } else {
            0
        };
        self.ctx.hvip = software | if self.timer.pending() { bits::VSTIP } else { 0 } | external;
        self.ctx.vsip = (self.ctx.vsip & !(bits::SSIP | bits::STIP | bits::SEIP))
            | if software != 0 { bits::SSIP } else { 0 }
            | if self.timer.pending() { bits::STIP } else { 0 }
            | if self.external_pending { bits::SEIP } else { 0 };
    }
    fn enabled_pending(&self) -> bool {
        self.ctx.vsie & self.ctx.vsip & (bits::SSIP | bits::STIP | bits::SEIP) != 0
    }
    /// # Safety
    /// H-capable IRQ-masked physical owner, immutable assigned G-stage backing,
    /// and a sole-owned physical timer. Resident/file state is put before any
    /// foreign control/fence/migration. Retire all visited CPUs before RAM reuse.
    pub unsafe fn run(&mut self, options: RunOptions<'_>) -> Result<ManagedExit, Error> {
        if options.host_deadline == u64::MAX {
            return Err(Error::InvalidDeadline);
        }
        self.refresh(crate::host::timer::read_time());
        // SAFETY: caller owns this CPU's compare endpoint through the complete interval.
        unsafe {
            options
                .physical
                .program(self.timer.physical_deadline(options.host_deadline))
        }
        .map_err(|_| Error::TimerUnavailable)?;
        let warm = options.resident.as_ref().is_some_and(|bank| bank.loaded());
        let residents =
            crate::vmid::entering_hart(self.resident_harts, crate::host::cpu_id(), || {
                // SAFETY: this pinned Zifencei hart acquired the published image before dispatch.
                unsafe { crate::entry::publish_guest_code() };
            });
        // SAFETY: exclusive context and immutable assigned backing; timer is armed.
        let raw = unsafe {
            match options.resident {
                Some(bank) => bank.run(&mut self.ctx),
                None => crate::entry::guest_run(&mut self.ctx),
            }
        }?;
        self.resident_harts = residents;
        if !raw.from_guest {
            return Err(Error::HostTransition);
        }
        self.translations.vmid = ((self.ctx.hgatp >> bits::VMID_SHIFT) & bits::VMID_MASK) as u16;
        if warm || self.ctx.tlb_flags == crate::vmid::TAGGED {
            self.translations.warm_entries += 1;
        } else {
            self.translations.entry_flushes += 1;
        }
        if self.ctx.guest_file != 0 {
            self.external_pending = self.ctx.vsip & bits::SEIP != 0;
        }
        let now = crate::host::timer::read_time();
        self.refresh(now);
        // SAFETY: raw is this run's trusted trap snapshot; callback observes checked accesses only.
        let (disposition, action, wfi, mmio) =
            unsafe { self.decode_exit(raw, options.space, options.io, now) };
        Ok(ManagedExit {
            raw,
            disposition,
            sbi: action,
            wfi,
            mmio,
            host_deadline_reached: crate::host::timer::read_time() >= options.host_deadline,
        })
    }
    unsafe fn decode_exit(
        &mut self,
        raw: VmExit,
        space: &GStagePageTable,
        io: &mut dyn GuestIo,
        now: u64,
    ) -> (Disposition, Option<SbiAction>, bool, bool) {
        if !raw.from_guest
            || (self.ctx.exit_hstatus & bits::HSTATUS_SPVP == 0 && !self.ctx.guest_os)
        {
            return (Disposition::Fault, None, false, false);
        }
        if matches!(
            raw.cause,
            VmExit::SUPERVISOR_TIMER | VmExit::SUPERVISOR_EXTERNAL
        ) {
            return (Disposition::Resume, None, false, false);
        }
        if raw.cause == VmExit::SUPERVISOR_SOFTWARE {
            crate::host::irq::clear_software_interrupt(
                #[cfg(feature = "selftest")]
                pauli_core::arch::NativeAckPath::GuestExit,
            );
            return (Disposition::Resume, None, false, false);
        }
        if raw.cause == VmExit::VS_ECALL {
            return self.sbi_exit(raw, now);
        }
        // SAFETY: checked trap/context and retained foreign instruction/table RAM.
        if unsafe { crate::wfi::is_wfi(&self.ctx, space, raw) } {
            self.ctx.pc += 4; // WFI decoder checked the addition before accepting it.
            return (
                if self.enabled_pending() {
                    Disposition::Resume
                } else {
                    Disposition::Wait
                },
                None,
                true,
                false,
            );
        }
        // SAFETY: decoder rejects implicit/VU/illegal accesses before invoking the callback.
        if unsafe { crate::mmio::emulate(&mut self.ctx, space, raw, &mut |access| io.mmio(access)) }
        {
            return (Disposition::Resume, None, false, true);
        }
        // SAFETY: provisioned AIA only; rejected decode/callback leaves PC and GPRs intact.
        if unsafe { crate::aia::emulation::emulate(&mut self.ctx, space, raw, io) } {
            return (Disposition::Resume, None, false, false);
        }
        (Disposition::Fault, None, false, false)
    }
    fn sbi_exit(&mut self, raw: VmExit, now: u64) -> (Disposition, Option<SbiAction>, bool, bool) {
        match sbi::handle_ecall_mode(&mut self.ctx, raw, self.smp) {
            Ok(action @ (SbiAction::Resume | SbiAction::SetTimer { .. })) => {
                if let SbiAction::SetTimer { deadline } = action {
                    self.timer.set_deadline(deadline, now);
                    self.refresh(now);
                }
                (Disposition::Resume, Some(action), false, false)
            }
            Ok(action @ (SbiAction::Shutdown { .. } | SbiAction::Reset { .. })) => {
                (Disposition::Shutdown, Some(action), false, false)
            }
            Ok(action) => (Disposition::Deferred, Some(action), false, false),
            Err(_) => (Disposition::Fault, None, false, false),
        }
    }
}
