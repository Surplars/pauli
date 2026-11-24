//! Acceptance-only F/D sentinel transition and original-host restoration.
//! Ownership/Lock Ordering: one IRQ-masked physical owner, no callbacks or locks.
//! Invariants: preserve the complete dormant host FP bank even when FS was Off.
/// Acceptance snapshot of the physical host bank, not the guest context image.
#[repr(C)]
pub struct HostProbe {
    pub fp: crate::fp::FloatState,
    pub status_before: usize,
    pub status_after: usize,
}
impl HostProbe {
    pub const fn new() -> Self {
        Self {
            fp: crate::fp::FloatState::new(),
            status_before: 0,
            status_after: 0,
        }
    }
}
const _: () = {
    assert!(core::mem::offset_of!(HostProbe, status_before) == 264);
    assert!(core::mem::offset_of!(HostProbe, status_after) == 272);
};
#[repr(C, align(16))]
struct ProbeFrame {
    fp: crate::fp::FloatState,
    ra: usize,
    observed: *mut HostProbe,
    status: usize,
}
core::arch::global_asm!(include_str!("fp_probe.S"),
    frame_bytes = const core::mem::size_of::<ProbeFrame>(),
    frame_fp = const core::mem::offset_of!(ProbeFrame, fp),
    frame_ra = const core::mem::offset_of!(ProbeFrame, ra),
    frame_observed = const core::mem::offset_of!(ProbeFrame, observed),
    frame_status = const core::mem::offset_of!(ProbeFrame, status),
    probe_fp = const core::mem::offset_of!(HostProbe, fp),
    probe_before = const core::mem::offset_of!(HostProbe, status_before),
    probe_after = const core::mem::offset_of!(HostProbe, status_after),
    fp_word = const core::mem::size_of::<u64>(),
    fp_fcsr = const core::mem::offset_of!(crate::fp::FloatState, fcsr),
    fs_mask = const crate::bits::SSTATUS_FS,
    fs_shift = const crate::bits::SSTATUS_FS.trailing_zeros(),
    full = const crate::resident::state::FULL,
);
unsafe extern "C" {
    fn pauli_guest_fp_probe(
        ctx: *mut crate::VCpuContext,
        sentinel: *const crate::fp::FloatState,
        observed: *mut HostProbe,
        host_fs: usize,
    );
}

/// Run the real transition with host FP sentinels, then restore the original
/// physical bank/FS before returning to Rust. No VM metadata is changed; inspect
/// ctx's raw exit fields. This fixture is not used by the production dispatcher.
/// # Safety
/// Same contract as guest_run, plus F/D hardware, host_fs in 0..=3 and valid
/// fcsr values. IRQs remain masked for the entire sentinel lifetime.
pub unsafe fn probe(
    ctx: &mut crate::VCpuContext,
    sentinel: &crate::fp::FloatState,
    observed: &mut HostProbe,
    host_fs: usize,
) -> Result<(), crate::Error> {
    // SAFETY: exclusive IRQ-masked owner, before taking the optional bank.
    unsafe { crate::vmid::prepare(ctx) }?;
    // SAFETY: this sentinel probe owns the discovered bank through restoration.
    let aia = unsafe { crate::aia::load(ctx.aia, ctx.virtual_aia && ctx.hvip & (1 << 10) != 0) };
    // SAFETY: validated provisioned F/D context, retained sentinel and exclusive output.
    unsafe { pauli_guest_fp_probe(ctx, sentinel, observed, host_fs) };
    if let Some(host) = aia {
        // SAFETY: same owner and matching saved bank, base-H was restored by assembly.
        unsafe {
            crate::aia::capture(ctx);
            host.restore();
        }
    }
    Ok(())
}
