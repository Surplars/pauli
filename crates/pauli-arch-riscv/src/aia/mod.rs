//! Guest APLIC/IMSIC models and discovered host Ssaia bank isolation.
//! Ownership/Lock Ordering: VM device -> file bank; hardware owner holds no VM lock.
//! Invariants: no physical file is exposed unless explicitly reserved for that vCPU.

use core::sync::atomic::{AtomicBool, Ordering};
pub mod aplic;
pub mod bank;
pub mod emulation;
pub mod file_bank;
pub mod file_state;
pub mod imsic;
#[cfg(target_arch = "riscv64")]
mod physical;
pub mod registers;
pub mod virtualized;
#[cfg(target_arch = "riscv64")]
pub(crate) use physical::{HostState, capture, load, set_external};
#[cfg(all(target_arch = "riscv64", feature = "aia-guest-file"))]
pub mod hardware;

static AVAILABLE: AtomicBool = AtomicBool::new(false);
pub const NEUTRAL_CONTROL: usize = 9 << 16;
pub const VGEIN_MASK: usize = 0x3f << 12;
pub fn init_capability(available: bool) {
    AVAILABLE.store(available, Ordering::Relaxed);
}
pub fn available() -> bool {
    AVAILABLE.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct GuestState {
    pub(super) select: usize,
}
impl GuestState {
    pub const fn new() -> Self {
        Self { select: 0 }
    }
}
