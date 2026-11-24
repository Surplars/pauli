//! Production algorithm acceptance, using private host fixtures.
//! Ownership/Lock Ordering: each test owns its state; no actual CSR access.
//! Invariants: retain the original protocol assertions and rejection cases.
use super::Statistics;
use super::state;
use crate::{VCpuContext, VmExit, entry};
use core::marker::PhantomData;

unsafe extern "C" {
    fn pauli_guest_load(host: *mut entry::HostBank);
    fn pauli_guest_put(host: *const entry::HostBank, plan: usize);
}

#[derive(Default)]
pub struct Resident {
    host: entry::HostBank,
    state: state::State,
    plan: usize,
    aia: Option<crate::aia::HostState>,
    external: bool,
    // A loaded bank cannot move to another physical owner via a Rust task.
    _local: PhantomData<*mut ()>,
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
    pub(crate) unsafe fn run(&mut self, ctx: &mut VCpuContext) -> Result<VmExit, crate::Error> {
        // SAFETY: same IRQ-masked physical owner; fail before taking an optional bank.
        unsafe { crate::vmid::prepare(ctx) }?;
        let external = ctx.virtual_aia && ctx.hvip & (1 << 10) != 0;
        if !self.state.loaded {
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
        let exit = unsafe { entry::enter(ctx, mode) };
        if self.aia.is_some() {
            // SAFETY: this IRQ-masked owner retains the matching context and saved base/optional banks until put.
            unsafe { crate::aia::capture(ctx) };
        }
        Ok(exit)
    }

    /// Restore the saved HS bank before releasing owner/context ownership.
    /// The complete guest snapshot was captured at the last exit, so put must
    /// NOT overwrite software-updated pending levels or a stopped CPU's reset.
    pub fn put(&mut self) {
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
        self.put();
    }
}
