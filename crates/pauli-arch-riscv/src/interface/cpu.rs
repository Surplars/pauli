//! vCPU trait adapter and translation of architecture-local exits to core actions.
//! Ownership/Lock Ordering: core hands over one owned off-CPU context, no locks.
//! Invariants: no guest CSR or SBI request can address native firmware resources.
use super::{RiscvResident, RiscvTimer, error};
use crate::{
    GStagePageTable, VCpuContext,
    sbi::{self, SbiAction},
    vcpu::VirtualCpu,
};
use pauli_core::{
    HvError, Result,
    arch::{self, ArchVcpu, Control, ControlError, ControlReply, ExitKind, GuestIo},
};
impl ArchVcpu for VirtualCpu {
    type Space = GStagePageTable;
    type Irq = crate::irq_backend::RiscvIrqChip;
    type Timer = RiscvTimer;
    type Resident = RiscvResident;
    type Mailbox = crate::remote::VcpuMailbox;
    fn create(
        space: &Self::Space,
        index: u8,
        epoch: u32,
        config: &pauli_config::VmConfig,
        boot: arch::VcpuBoot,
    ) -> Result<Self> {
        let mut ctx = VCpuContext::new();
        ctx.pc = boot.entry;
        ctx.x[10] = index as usize;
        ctx.x[11] = boot.opaque;
        if boot.payload != pauli_core::board::Payload::Embedded {
            ctx.enable_guest_os();
            // Integer firmware needs VS traps, not an F/D register-bank switch.
            if boot.payload == pauli_core::board::Payload::Linux {
                if crate::fp::available() {
                    ctx.enable_floating_point().map_err(error)?;
                }
                if crate::sstc_available() {
                    ctx.enable_sstc().map_err(error)?;
                }
            }
        }
        if config.aia {
            ctx.enable_virtual_aia().map_err(error)?;
        }
        Self::new(epoch, index, config.vcpus, ctx, space).map_err(error)
    }
    fn reset_secondary(&mut self, space: &Self::Space, entry: usize, opaque: usize) -> Result<()> {
        Self::reset_secondary(self, entry, opaque, space).map_err(error)
    }
    unsafe fn run(&mut self, opts: arch::RunOptions<'_, Self>) -> Result<arch::VcpuExit> {
        // SAFETY: forward the caller's exclusive run contract.
        unsafe { self.run_with_trace(opts, None) }
    }
    unsafe fn run_with_trace(
        &mut self,
        opts: arch::RunOptions<'_, Self>,
        mut sample: Option<&mut arch::GuestSample>,
    ) -> Result<arch::VcpuExit> {
        let mut io = Io {
            inner: opts.io,
            cpu: self.index(),
        };
        // SAFETY: core transfers the IRQ-masked native owner with retained space and sole timer.
        let exit = unsafe {
            Self::run_with_interval(
                self,
                crate::vcpu::RunOptions {
                    space: opts.space,
                    host_deadline: opts.deadline,
                    io: &mut io,
                    physical: &mut opts.timer.0,
                    resident: Some(&mut opts.resident.bank),
                },
                sample.as_deref_mut().map(|s| &mut s.interval),
            )
        }
        .map_err(error)?;
        let kind = match exit.disposition {
            crate::vcpu::Disposition::Resume => ExitKind::Resume,
            crate::vcpu::Disposition::Wait => ExitKind::Wait,
            crate::vcpu::Disposition::Fault => ExitKind::Fault,
            _ => action(exit.sbi.ok_or(HvError::InvalidState)?)?,
        };
        if let Some(sample) = sample {
            sample.source = crate::VmExit::source(exit.raw.cause, exit.wfi, exit.mmio);
        }
        Ok(arch::VcpuExit {
            kind,
            cause: exit.raw.cause,
            pc: exit.raw.pc,
            value: exit.raw.value,
            deadline_reached: exit.host_deadline_reached,
            native_event: matches!(
                exit.raw.cause,
                crate::VmExit::SUPERVISOR_SOFTWARE | crate::VmExit::SUPERVISOR_EXTERNAL
            ),
        })
    }
    fn set_external_irq(&mut self, asserted: bool) {
        self.set_external_line(asserted);
    }
    fn inject_ipi(&mut self) {
        self.inject_software_interrupt();
    }
    fn wait_deadline(&self) -> Option<u64> {
        Self::wait_deadline(self)
    }
    fn wake_pending(&mut self) -> bool {
        self.poll_wakeup()
    }
    fn finish_control(&mut self, reply: ControlReply) -> Result<()> {
        let (code, value) = match reply {
            #[cfg(feature = "guest-control")]
            ControlReply::Transport { error, value } => (error as isize, value as usize),
            ControlReply::Success(value) => (0, value),
            ControlReply::CpuStatus(state) => (
                0,
                match state {
                    arch::CpuStatus::Started => 0,
                    arch::CpuStatus::Stopped => 1,
                    arch::CpuStatus::StartPending => 2,
                    arch::CpuStatus::StopPending => 3,
                },
            ),
            ControlReply::Error(kind) => (
                match kind {
                    ControlError::Failed => sbi::FAILED,
                    ControlError::Unsupported => sbi::NOT_SUPPORTED,
                    ControlError::InvalidParameter => sbi::INVALID_PARAM,
                    ControlError::InvalidAddress => sbi::INVALID_ADDRESS,
                    ControlError::AlreadyAvailable => sbi::ALREADY_AVAILABLE,
                },
                0,
            ),
        };
        self.finish_sbi(code, value).map_err(error)
    }
    unsafe fn synchronize(&mut self, instruction: bool) {
        // SAFETY: core grants the off-CPU pinned context with resident banks put.
        unsafe {
            self.synchronize_translations(crate::remote::FenceBatch::single(instruction));
        }
    }
    fn retire_local_translations(&mut self) {
        Self::retire_local_translations(self);
    }
    fn translations_retired(&self) -> bool {
        Self::translations_retired(self)
    }
}
struct Io<'a> {
    inner: &'a mut dyn GuestIo,
    cpu: u8,
}
impl crate::vcpu::GuestIo for Io<'_> {
    fn mmio(&mut self, access: pauli_hal::device::MmioAccess) -> Option<usize> {
        self.inner.mmio(access)
    }
    fn csr(&mut self, access: crate::aia::emulation::CsrAccess) -> Option<usize> {
        self.inner.register(
            self.cpu,
            arch::RegisterAccess {
                register: access.register,
                select: access.select,
                clear: access.clear,
                set: access.set,
                write: access.write,
            },
        )
    }
}
fn action(action: SbiAction) -> Result<ExitKind> {
    Ok(match action {
        SbiAction::Shutdown { .. } => ExitKind::Shutdown,
        SbiAction::Reset { .. } => ExitKind::Reset,
        SbiAction::HartStart {
            hart,
            entry,
            opaque,
        } => ExitKind::Control(Control::Start {
            cpu: hart,
            entry,
            opaque,
        }),
        SbiAction::HartStop => ExitKind::Control(Control::StopCpu),
        SbiAction::HartStatus { hart } => ExitKind::Control(Control::CpuStatus { cpu: hart }),
        SbiAction::SendIpi { mask, base } => ExitKind::Control(Control::Ipi { mask, base }),
        SbiAction::RemoteFence {
            mask,
            base,
            instruction,
        } => ExitKind::Control(Control::Fence {
            mask,
            base,
            instruction,
        }),
        #[cfg(feature = "guest-control")]
        SbiAction::ManagementProbe => ExitKind::Control(Control::ManagementProbe),
        #[cfg(feature = "guest-control")]
        SbiAction::Management { args } => ExitKind::Control(Control::Management { args }),
        _ => return Err(HvError::InvalidState),
    })
}
impl arch::Mailbox<VirtualCpu> for crate::remote::VcpuMailbox {
    type Ticket = crate::remote::FenceTicket;
    fn new(epoch: u32, index: u8, owner: usize) -> Self {
        Self::new(epoch, index, owner)
    }
    fn post_ipi(&self) {
        Self::post_ipi(self);
    }
    fn post_fence(&self, instruction: bool, requester: u8) -> Self::Ticket {
        Self::post_fence(self, instruction, requester)
    }
    fn completed(&self, ticket: Self::Ticket) -> bool {
        Self::completed(self, ticket)
    }
    #[cfg(feature = "selftest")]
    fn fence_sample(&self, instruction: bool, generation: usize) -> Option<arch::FenceSample> {
        Self::fence_sample(self, instruction, generation)
    }
    #[cfg(feature = "selftest")]
    fn note_fence_ready(&self, ticket: Self::Ticket, requester: u8, deferred: bool) {
        self.note_ready(ticket, requester, deferred);
    }
    fn disarm(&self, requester: u8) {
        self.disarm_fence_notify(requester);
    }
    fn pending(&self) -> bool {
        Self::pending(self)
    }
    unsafe fn drain(&self, vcpu: &mut VirtualCpu) -> Result<usize> {
        // SAFETY: core grants matching pinned off-CPU context, with IRQs masked and banks put.
        unsafe { Self::drain(self, vcpu) }.map_err(error)
    }
}
