#![no_std]
//! RV64 H/HS backend and host-testable RISC-V translation/interrupt protocols.
//! Ownership/Lock Ordering: physical owner excludes IRQs on entry; no VM registry here.
//! Invariants: immutable G-stage backing, full FP isolation and retirement before RAM reuse.

pub mod aia;
pub mod bits;
pub mod context;
pub mod exit;
pub mod fp;
pub mod gstage;
#[cfg(any(test, all(target_arch = "riscv64", feature = "selftest")))]
mod idle_wake;
#[cfg(target_arch = "riscv64")]
pub mod interface;
pub mod interrupt;
pub mod irq_backend;
pub mod mmio;
#[cfg(all(feature = "selftest", any(test, target_arch = "riscv64")))]
mod native_ack;
#[cfg(any(target_arch = "riscv64", test))]
mod native_harts;
pub mod plic;
pub mod remote;
pub mod sbi;
pub mod timer;
pub mod vmid;
pub mod vstage;
pub mod wfi;

#[cfg(target_arch = "riscv64")]
pub mod entry;
#[cfg(target_arch = "riscv64")]
pub mod host;
pub mod resident;
#[cfg(all(target_arch = "riscv64", feature = "selftest"))]
pub mod selftest;
#[cfg(target_arch = "riscv64")]
pub mod vcpu;

pub use context::VCpuContext;
pub use exit::VmExit;
pub use gstage::GStagePageTable;
pub const MAX_CPUS: usize = pauli_config::MAX_HOST_CPUS as usize;
#[cfg(target_arch = "riscv64")]
const _: () = assert!(MAX_CPUS <= usize::BITS as usize); // VMID and IMSIC owner bitmaps.
pub const MAX_VCPUS: usize = pauli_config::MAX_GUEST_VCPUS as usize;
pub const VS_PAGE_FAULTS: usize = (1 << 12) | (1 << 13) | (1 << 15);
pub const VS_OS_EXCEPTIONS: usize = VS_PAGE_FAULTS | 0x1ff;
static PRESENT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn available() -> bool {
    PRESENT.load(core::sync::atomic::Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Unsupported,
    InvalidImage,
    InvalidState,
    InvalidDeadline,
    TimerUnavailable,
    Busy,
    InvalidVcpu,
    HostTransition,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmResetKind {
    Cold,
    Warm,
}

pub fn init_capabilities(hypervisor: bool, floating_point: bool, ssaia: bool) {
    PRESENT.store(hypervisor, core::sync::atomic::Ordering::Relaxed);
    fp::init_capability(hypervisor && floating_point);
    aia::init_capability(hypervisor && ssaia);
}

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests;
