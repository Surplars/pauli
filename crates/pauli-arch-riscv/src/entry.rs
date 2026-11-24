//! Fixed-layout RV64 world switch, with no VM table or profiling side effects.
//! Ownership/Lock Ordering: one IRQ-masked native owner; no locks across SRET.
//! Invariants: unconditional provisioned F/D swap; VGEIN only from a bound private file.
use crate::{VCpuContext, VmExit, bits};
mod frame;
pub(crate) use frame::HostBank;
use frame::HostFrame;

/// Synchronize instruction fetch on this hart only; this does not flush a peer.
/// # Safety
/// Zifencei is present; the image stores are already visible to this hart.
pub unsafe fn publish_guest_code() {
    // SAFETY: caller owns instruction publication on this native CPU.
    unsafe {
        core::arch::asm!("fence.i", options(nostack));
    }
}

core::arch::global_asm!(
    include_str!("entry.S"), include_str!("fp_macros.S"), include_str!("resident.S"),
    include_str!("entry/enter.S"), include_str!("entry/exit.S"),
    ctx_x1 = const core::mem::offset_of!(VCpuContext, x) + 1 * core::mem::size_of::<usize>(),
    ctx_x2 = const core::mem::offset_of!(VCpuContext, x) + 2 * core::mem::size_of::<usize>(),
    ctx_x3 = const core::mem::offset_of!(VCpuContext, x) + 3 * core::mem::size_of::<usize>(),
    ctx_x4 = const core::mem::offset_of!(VCpuContext, x) + 4 * core::mem::size_of::<usize>(),
    ctx_x5 = const core::mem::offset_of!(VCpuContext, x) + 5 * core::mem::size_of::<usize>(),
    ctx_x6 = const core::mem::offset_of!(VCpuContext, x) + 6 * core::mem::size_of::<usize>(),
    ctx_x7 = const core::mem::offset_of!(VCpuContext, x) + 7 * core::mem::size_of::<usize>(),
    ctx_x8 = const core::mem::offset_of!(VCpuContext, x) + 8 * core::mem::size_of::<usize>(),
    ctx_x9 = const core::mem::offset_of!(VCpuContext, x) + 9 * core::mem::size_of::<usize>(),
    ctx_x10 = const core::mem::offset_of!(VCpuContext, x) + 10 * core::mem::size_of::<usize>(),
    ctx_x11 = const core::mem::offset_of!(VCpuContext, x) + 11 * core::mem::size_of::<usize>(),
    ctx_x12 = const core::mem::offset_of!(VCpuContext, x) + 12 * core::mem::size_of::<usize>(),
    ctx_x13 = const core::mem::offset_of!(VCpuContext, x) + 13 * core::mem::size_of::<usize>(),
    ctx_x14 = const core::mem::offset_of!(VCpuContext, x) + 14 * core::mem::size_of::<usize>(),
    ctx_x15 = const core::mem::offset_of!(VCpuContext, x) + 15 * core::mem::size_of::<usize>(),
    ctx_x16 = const core::mem::offset_of!(VCpuContext, x) + 16 * core::mem::size_of::<usize>(),
    ctx_x17 = const core::mem::offset_of!(VCpuContext, x) + 17 * core::mem::size_of::<usize>(),
    ctx_x18 = const core::mem::offset_of!(VCpuContext, x) + 18 * core::mem::size_of::<usize>(),
    ctx_x19 = const core::mem::offset_of!(VCpuContext, x) + 19 * core::mem::size_of::<usize>(),
    ctx_x20 = const core::mem::offset_of!(VCpuContext, x) + 20 * core::mem::size_of::<usize>(),
    ctx_x21 = const core::mem::offset_of!(VCpuContext, x) + 21 * core::mem::size_of::<usize>(),
    ctx_x22 = const core::mem::offset_of!(VCpuContext, x) + 22 * core::mem::size_of::<usize>(),
    ctx_x23 = const core::mem::offset_of!(VCpuContext, x) + 23 * core::mem::size_of::<usize>(),
    ctx_x24 = const core::mem::offset_of!(VCpuContext, x) + 24 * core::mem::size_of::<usize>(),
    ctx_x25 = const core::mem::offset_of!(VCpuContext, x) + 25 * core::mem::size_of::<usize>(),
    ctx_x26 = const core::mem::offset_of!(VCpuContext, x) + 26 * core::mem::size_of::<usize>(),
    ctx_x27 = const core::mem::offset_of!(VCpuContext, x) + 27 * core::mem::size_of::<usize>(),
    ctx_x28 = const core::mem::offset_of!(VCpuContext, x) + 28 * core::mem::size_of::<usize>(),
    ctx_x29 = const core::mem::offset_of!(VCpuContext, x) + 29 * core::mem::size_of::<usize>(),
    ctx_x30 = const core::mem::offset_of!(VCpuContext, x) + 30 * core::mem::size_of::<usize>(),
    ctx_x31 = const core::mem::offset_of!(VCpuContext, x) + 31 * core::mem::size_of::<usize>(),
    ctx_pc = const core::mem::offset_of!(VCpuContext, pc),
    ctx_vsstatus = const core::mem::offset_of!(VCpuContext, vsstatus),
    ctx_vsatp = const core::mem::offset_of!(VCpuContext, vsatp),
    ctx_vstvec = const core::mem::offset_of!(VCpuContext, vstvec),
    ctx_vsscratch = const core::mem::offset_of!(VCpuContext, vsscratch),
    ctx_vsepc = const core::mem::offset_of!(VCpuContext, vsepc),
    ctx_vscause = const core::mem::offset_of!(VCpuContext, vscause),
    ctx_vstval = const core::mem::offset_of!(VCpuContext, vstval),
    ctx_vsie = const core::mem::offset_of!(VCpuContext, vsie),
    ctx_vsip = const core::mem::offset_of!(VCpuContext, vsip),
    ctx_hgatp = const core::mem::offset_of!(VCpuContext, hgatp),
    ctx_exit_cause = const core::mem::offset_of!(VCpuContext, exit_cause),
    ctx_exit_value = const core::mem::offset_of!(VCpuContext, exit_value),
    ctx_exit_gpa_shifted = const core::mem::offset_of!(VCpuContext, exit_gpa_shifted),
    ctx_exit_instruction = const core::mem::offset_of!(VCpuContext, exit_instruction),
    ctx_exit_hstatus = const core::mem::offset_of!(VCpuContext, exit_hstatus),
    ctx_host_sp = const core::mem::offset_of!(VCpuContext, host_sp),
    ctx_senvcfg = const core::mem::offset_of!(VCpuContext, senvcfg),
    ctx_scounteren = const core::mem::offset_of!(VCpuContext, scounteren),
    ctx_hvip = const core::mem::offset_of!(VCpuContext, hvip),
    ctx_hedeleg = const core::mem::offset_of!(VCpuContext, hedeleg),
    ctx_resume_vs = const core::mem::offset_of!(VCpuContext, resume_vs),
    ctx_hideleg = const core::mem::offset_of!(VCpuContext, hideleg),
    ctx_fp = const core::mem::offset_of!(VCpuContext, fp),
    ctx_fp_enabled = const core::mem::offset_of!(VCpuContext, fp_enabled),
    ctx_tlb_flags = const core::mem::offset_of!(VCpuContext, tlb_flags),
    ctx_guest_file = const core::mem::offset_of!(VCpuContext, guest_file),
    host_ra = const core::mem::offset_of!(HostFrame, registers) + 0 * core::mem::size_of::<usize>(),
    host_s0 = const core::mem::offset_of!(HostFrame, registers) + 1 * core::mem::size_of::<usize>(),
    host_s1 = const core::mem::offset_of!(HostFrame, registers) + 2 * core::mem::size_of::<usize>(),
    host_s2 = const core::mem::offset_of!(HostFrame, registers) + 3 * core::mem::size_of::<usize>(),
    host_s3 = const core::mem::offset_of!(HostFrame, registers) + 4 * core::mem::size_of::<usize>(),
    host_s4 = const core::mem::offset_of!(HostFrame, registers) + 5 * core::mem::size_of::<usize>(),
    host_s5 = const core::mem::offset_of!(HostFrame, registers) + 6 * core::mem::size_of::<usize>(),
    host_s6 = const core::mem::offset_of!(HostFrame, registers) + 7 * core::mem::size_of::<usize>(),
    host_s7 = const core::mem::offset_of!(HostFrame, registers) + 8 * core::mem::size_of::<usize>(),
    host_s8 = const core::mem::offset_of!(HostFrame, registers) + 9 * core::mem::size_of::<usize>(),
    host_s9 = const core::mem::offset_of!(HostFrame, registers) + 10 * core::mem::size_of::<usize>(),
    host_s10 = const core::mem::offset_of!(HostFrame, registers) + 11 * core::mem::size_of::<usize>(),
    host_s11 = const core::mem::offset_of!(HostFrame, registers) + 12 * core::mem::size_of::<usize>(),
    host_gp = const core::mem::offset_of!(HostFrame, registers) + 13 * core::mem::size_of::<usize>(),
    host_tp = const core::mem::offset_of!(HostFrame, registers) + 14 * core::mem::size_of::<usize>(),
    host_a0 = const core::mem::offset_of!(HostFrame, registers) + 15 * core::mem::size_of::<usize>(),
    host_sstatus = const core::mem::offset_of!(HostFrame, sstatus),
    host_stvec = const core::mem::offset_of!(HostFrame, stvec),
    host_sscratch = const core::mem::offset_of!(HostFrame, sscratch),
    host_sie = const core::mem::offset_of!(HostFrame, sie),
    host_sepc = const core::mem::offset_of!(HostFrame, sepc),
    host_hstatus = const core::mem::offset_of!(HostFrame, hstatus),
    host_bank = const core::mem::offset_of!(HostFrame, bank),
    host_scause = const core::mem::offset_of!(HostFrame, scause),
    host_stval = const core::mem::offset_of!(HostFrame, stval),
    host_htval = const core::mem::offset_of!(HostFrame, htval),
    host_htinst = const core::mem::offset_of!(HostFrame, htinst),
    host_senvcfg = const core::mem::offset_of!(HostFrame, senvcfg),
    host_scounteren = const core::mem::offset_of!(HostFrame, scounteren),
    host_mode = const core::mem::offset_of!(HostFrame, mode),
    host_fp = const core::mem::offset_of!(HostFrame, fp),
    bank_hedeleg = const core::mem::offset_of!(HostBank, hedeleg),
    bank_hideleg = const core::mem::offset_of!(HostBank, hideleg),
    bank_hgatp = const core::mem::offset_of!(HostBank, hgatp),
    bank_hvip = const core::mem::offset_of!(HostBank, hvip),
    bank_hcounteren = const core::mem::offset_of!(HostBank, hcounteren),
    bank_henvcfg = const core::mem::offset_of!(HostBank, henvcfg),
    bank_vsstatus = const core::mem::offset_of!(HostBank, vsstatus),
    bank_vsatp = const core::mem::offset_of!(HostBank, vsatp),
    bank_vstvec = const core::mem::offset_of!(HostBank, vstvec),
    bank_vsscratch = const core::mem::offset_of!(HostBank, vsscratch),
    bank_vsepc = const core::mem::offset_of!(HostBank, vsepc),
    bank_vscause = const core::mem::offset_of!(HostBank, vscause),
    bank_vstval = const core::mem::offset_of!(HostBank, vstval),
    bank_htimedelta = const core::mem::offset_of!(HostBank, htimedelta),
    bank_hie = const core::mem::offset_of!(HostBank, hie),
    host_bytes = const core::mem::size_of::<HostFrame>(),
    fp_fcsr = const core::mem::offset_of!(crate::fp::FloatState, fcsr),
    fp_word = const core::mem::size_of::<u64>(),
    fp_vector_mask = const bits::SSTATUS_FS | bits::SSTATUS_VS,
    fs_dirty = const bits::SSTATUS_FS,
    delegated_exceptions = const crate::VS_OS_EXCEPTIONS,
    virtual_pending = const bits::VIRTUAL_PENDING,
    counter_time = const bits::COUNTER_TIME,
    tlb_invalidate = const crate::vmid::policy::INVALIDATE,
    tlb_tagged = const crate::vmid::policy::TAGGED,
    vmid_shift = const bits::VMID_SHIFT,
    vmid_mask = const bits::VMID_MASK,
    hstatus_file_privilege_mask = const bits::HSTATUS_VGEIN | bits::HSTATUS_SPVP,
    hstatus_guest_base = const bits::HSTATUS_VTW | bits::HSTATUS_SPV,
    hstatus_vgein_shift = const bits::HSTATUS_VGEIN_SHIFT,
    privilege_shift = const bits::SSTATUS_SPP.trailing_zeros(),
    privilege_bit = const 1,
    sstatus_spp = const bits::SSTATUS_SPP,
    host_interrupts = const bits::HOST_GUEST_INTERRUPTS,
    resident_resume = const crate::resident::state::RESUME,
    guest_file_enabled = const cfg!(feature = "aia-guest-file") as usize,
);
unsafe extern "C" {
    fn pauli_guest_enter(ctx: *mut VCpuContext, mode: usize);
}

/// # Safety
/// H-capable HS owner, IRQs masked, exclusive vCPU and immutable retained private
/// G-stage backing. A physical preemption deadline is armed. Provisioned F/D
/// and Ssaia require matching discovered CPU capabilities. Retire every visited
/// owner's translations before destroying or rewriting any mapped backing.
pub unsafe fn guest_run(ctx: &mut VCpuContext) -> Result<VmExit, crate::Error> {
    // SAFETY: exclusive IRQ-masked physical owner and immutable epoch/root.
    unsafe {
        crate::vmid::prepare(ctx)?;
    }
    // SAFETY: the caller owns the discovered optional bank until restoration.
    let bank = unsafe { crate::aia::load(ctx.aia, ctx.virtual_aia && ctx.hvip & bits::VSEIP != 0) };
    // SAFETY: valid exclusive context/backing and an armed preemption deadline.
    let exit = unsafe { enter(ctx, crate::resident::state::FULL) };
    if let Some(bank) = bank {
        // SAFETY: guest has exited; capture before restoring the matching HS bank.
        unsafe {
            crate::aia::capture(ctx);
            bank.restore();
        }
    }
    Ok(exit)
}

/// # Safety
/// Same guest_run contract; non-FULL mode additionally owns the loaded resident bank.
pub(crate) unsafe fn enter(ctx: &mut VCpuContext, mode: usize) -> VmExit {
    // SAFETY: exclusive live context and matching mode/physical-owner bank contract.
    unsafe {
        pauli_guest_enter(ctx, mode);
    }
    VmExit {
        cause: ctx.exit_cause,
        value: ctx.exit_value,
        gpa_shifted: ctx.exit_gpa_shifted,
        instruction: ctx.exit_instruction,
        pc: ctx.pc,
        from_guest: ctx.exit_hstatus & bits::HSTATUS_SPV != 0,
    }
}
