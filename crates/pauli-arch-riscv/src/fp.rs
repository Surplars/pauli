//! FLEN=64 guest context. No allocator, native-task ABI change or lazy owner.
//! Ownership/Lock Ordering: boot publishes capability; each vCPU owns its entire FP bank.
//! Invariants: private provisioning policy, never guest FS, controls unconditional swapping.
use core::sync::atomic::{AtomicBool, Ordering};

static AVAILABLE: AtomicBool = AtomicBool::new(false);

pub(super) fn init_capability(fd: bool) {
    AVAILABLE.store(fd, Ordering::Relaxed);
}

/// Firmware must advertise F/D without Q on every eligible hart. Read only at
/// provisioning/acceptance time; the assembly transition uses private policy.
pub fn available() -> bool {
    AVAILABLE.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct FloatState {
    pub registers: [u64; 32],
    pub fcsr: usize,
}

impl FloatState {
    pub const fn new() -> Self {
        Self {
            registers: [0; 32],
            fcsr: 0,
        }
    }
}

const _: () = {
    assert!(core::mem::offset_of!(FloatState, fcsr) == 256);
    assert!(core::mem::size_of::<FloatState>() == 264);
};
