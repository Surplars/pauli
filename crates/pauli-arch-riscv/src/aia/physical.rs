//! Native adapter for the host-testable Ssaia bank protocol.
//! Ownership/Lock Ordering: one IRQ-masked physical owner, no locks.
//! Invariants: Native exists only within a discovered, exclusively owned bank.
use super::{
    GuestState, available,
    bank::{self, Register, Registers},
};
use crate::host::aia_csr::hypervisor as csr;

struct Native;
impl Registers for Native {
    fn read(&mut self, register: Register) -> usize {
        // SAFETY: Native is private and only constructed by the owned-bank entrypoints below.
        unsafe {
            match register {
                Register::Enable => csr::hvien::read(),
                Register::Control => csr::hvictl::read(),
                Register::Select => csr::vsiselect::read(),
                Register::Priority1 => csr::hviprio1::read(),
                Register::Priority2 => csr::hviprio2::read(),
                Register::Vsie => csr::vsie::read(),
            }
        }
    }
    fn write(&mut self, register: Register, value: usize) {
        // SAFETY: matching discovered, IRQ-masked native bank owns all six registers.
        unsafe {
            match register {
                Register::Enable => csr::hvien::write(value),
                Register::Control => csr::hvictl::write(value),
                Register::Select => csr::vsiselect::write(value),
                Register::Priority1 => csr::hviprio1::write(value),
                Register::Priority2 => csr::hviprio2::write(value),
                Register::Vsie => csr::vsie::write(value),
            }
        }
    }
}
pub(crate) struct HostState(bank::HostState);
/// # Safety
/// IRQ-masked H+Ssaia owner; restore base-H before this optional bank.
pub(crate) unsafe fn load(guest: GuestState, external: bool) -> Option<HostState> {
    bank::load(&mut Native, available(), guest, external).map(HostState)
}
/// # Safety
/// Discovered H+Ssaia bank, matching load owner, IRQs still masked.
pub(crate) unsafe fn set_external(external: bool) {
    bank::set_external(&mut Native, external);
}
/// # Safety
/// Guest has exited on the matching H+Ssaia owner; IRQs remain masked.
pub(crate) unsafe fn capture(ctx: &mut crate::VCpuContext) {
    ctx.aia = bank::snapshot(&mut Native);
}
impl HostState {
    /// # Safety
    /// Same IRQ-masked owner, after base-H restoration, before native IRQs.
    pub(crate) unsafe fn restore(self) {
        self.0.restore(&mut Native);
    }
}
