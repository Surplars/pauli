//! Production resident bank ownership across quick world-switch exits.
//! Ownership/Lock Ordering: one IRQ-masked owner; no locks or foreign callbacks.
//! Invariants: host CSR, optional F/D, and AIA banks are always restored before drop.
use super::Statistics;
use super::state;
use crate::{VCpuContext, VmExit, entry, fp::FloatState};
use core::marker::PhantomData;

unsafe extern "C" {
    fn pauli_guest_load(host: *mut entry::HostBank);
    fn pauli_guest_put(host: *const entry::HostBank, plan: usize);
    fn pauli_fp_load(host_fp: *mut FloatState, host_fs: *mut usize, guest_fp: *const FloatState);
    fn pauli_fp_put(guest_fp: *mut FloatState, host_fp: *const FloatState, host_fs: usize);
    fn pauli_fp_restore_host(host_fp: *const FloatState, host_fs: usize);
}

pub struct Resident {
    host: entry::HostBank,
    host_fp: FloatState,
    host_fs: usize,
    state: state::State,
    plan: usize,
    aia: Option<crate::aia::HostState>,
    external: bool,
    // A loaded bank cannot move to another physical owner via a Rust task.
    _local: PhantomData<*mut ()>,
}
impl Default for Resident {
    fn default() -> Self {
        Self {
            host: entry::HostBank::default(),
            host_fp: FloatState::new(),
            host_fs: 0,
            state: state::State::default(),
            plan: 0,
            aia: None,
            external: false,
            _local: PhantomData,
        }
    }
}
impl Resident {
    pub fn statistics(&self) -> Statistics {
        self.state.statistics
    }
    pub(crate) fn loaded(&self) -> bool {
        self.state.loaded
    }

    /// # Safety
    /// Same contract as guest_run, plus a dedicated owner with IRQs masked
    /// until put. Between entries only GPR/PC and pending IRQ state may change
    /// in memory. A context reset, foreign control/fence service, migration or
    /// native scheduling requires put FIRST. The same vCPU exclusively owns
    /// the bank until then; neither callback code nor firmware may replace it.
    pub(crate) unsafe fn run(
        &mut self,
        ctx: &mut VCpuContext,
        interval: Option<&mut pauli_core::arch::TickInterval>,
    ) -> Result<VmExit, crate::Error> {
        if !self.state.loaded {
            ctx.check_sstc_owner(crate::local_sstc_available)?;
        }
        // SAFETY: same IRQ-masked physical owner; fail before taking an optional bank.
        unsafe { crate::vmid::prepare(ctx) }?;
        let external = ctx.virtual_aia && ctx.hvip & (1 << 10) != 0;
        if !self.state.loaded {
            if self.state.load_fp(ctx.fp_enabled != 0) {
                // SAFETY: IRQ-masked owner retains exclusive physical F/D registers
                // and private `host_fp`/`host_fs` storage until `put`/`drop`.
                unsafe { pauli_fp_load(&mut self.host_fp, &mut self.host_fs, &ctx.fp) };
            }
            // SAFETY: this IRQ-masked owner retains the matching context and saved base/optional banks until put.
            self.aia = unsafe { crate::aia::load(ctx.aia, external) };
            self.external = external;
            // SAFETY: this IRQ-masked owner retains the matching context and saved base/optional banks until put.
            unsafe { pauli_guest_load(&mut self.host) };
        } else if self.aia.is_some() && self.external != external {
            // SAFETY: this IRQ-masked owner retains the matching context and saved base/optional banks until put.
            unsafe { crate::aia::set_external(external) };
            self.external = external;
        }
        self.plan = ctx.tlb_flags;
        let mode = self.state.enter();
        // SAFETY: this IRQ-masked owner retains the matching context and saved base/optional banks until put.
        let exit = unsafe { entry::enter(ctx, mode, interval) };
        if self.aia.is_some() {
            // SAFETY: this IRQ-masked owner retains the matching context and saved base/optional banks until put.
            unsafe { crate::aia::capture(ctx) };
        }
        Ok(exit)
    }

    /// Restore the saved HS bank and save/restore F/D before releasing ownership.
    /// The complete guest snapshot was captured at the last exit, so put must
    /// NOT overwrite software-updated pending levels or a stopped CPU's reset.
    pub fn put(&mut self, ctx: &mut VCpuContext) {
        if self.state.put_fp() {
            // SAFETY: guest exited; same IRQ-masked owner saves guest F/D and restores host bank.
            unsafe { pauli_fp_put(&mut ctx.fp, &self.host_fp, self.host_fs) };
        }
        self.put_base_and_aia();
    }

    fn put_base_and_aia(&mut self) {
        if self.state.put() {
            // SAFETY: this IRQ-masked owner retains the matching context and saved base/optional banks until put.
            unsafe { pauli_guest_put(&self.host, self.plan) };
            if let Some(host) = self.aia.take() {
                // SAFETY: this IRQ-masked owner retains the matching context and saved base/optional banks until put.
                unsafe { host.restore() };
            }
        }
    }
}
impl Drop for Resident {
    fn drop(&mut self) {
        if self.state.put_fp() {
            // SAFETY: fallback cleanup restores host F/D registers and FS before dropping.
            unsafe { pauli_fp_restore_host(&self.host_fp, self.host_fs) };
        }
        self.put_base_and_aia();
    }
}
