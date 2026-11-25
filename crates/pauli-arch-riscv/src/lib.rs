#![no_std]
//! RV64 H/HS backend and host-testable RISC-V translation/interrupt protocols.
//! Ownership/Lock Ordering: physical owner excludes IRQs on entry; no VM registry here.
//! Invariants: immutable G-stage backing, full FP isolation and retirement before RAM reuse.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
static PRESENT: AtomicBool = AtomicBool::new(false);

/// A set bit identifies a logical native owner that completed its compare probe.
static SSTC_OWNERS: AtomicUsize = AtomicUsize::new(0);

fn owner_bit(owner: usize) -> usize {
    if owner < MAX_CPUS.min(usize::BITS as usize) {
        1usize << owner
    } else {
        0
    }
}

fn owners_have_sstc(owners: &AtomicUsize, mask: usize) -> bool {
    mask != 0 && owners.load(Ordering::Acquire) & mask == mask
}

#[cfg(any(target_arch = "riscv64", test))]
fn probe_owner_sstc(
    owners: &AtomicUsize,
    owner: usize,
    gates: bool,
    disarm: impl FnOnce() -> bool,
) -> bool {
    let bit = owner_bit(owner);
    if bit == 0 {
        return false;
    }
    let supported = gates && disarm();
    if supported {
        owners.fetch_or(bit, Ordering::Release);
    } else {
        owners.fetch_and(!bit, Ordering::Release);
    }
    supported
}

pub fn available() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

pub fn local_sstc_available() -> bool {
    #[cfg(target_arch = "riscv64")]
    let owner = host::cpu_id();
    #[cfg(not(target_arch = "riscv64"))]
    let owner = 0;
    sstc_available_for_owners(owner_bit(owner))
}

/// Guest exposure requires every owner in its nonempty placement mask to qualify.
pub fn sstc_available_for_owners(mask: usize) -> bool {
    owners_have_sstc(&SSTC_OWNERS, mask)
}

/// Capability of the calling native hart.
pub fn sstc_available() -> bool {
    local_sstc_available()
}

#[cfg(test)]
fn init_sstc(available: bool) {
    probe_owner_sstc(&SSTC_OWNERS, 0, available, || true);
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

#[cfg(target_arch = "riscv64")]
unsafe fn probe_sstc_gates() -> bool {
    let (env, counters): (usize, usize);
    // The base H switch already requires access to both CSRs. Test their WARL
    // bits first, without assuming firmware enabled supervisor compare access.
    unsafe {
        core::arch::asm!(
            ".option push",
            ".option arch, +h",
            "csrrs {old}, henvcfg, {stce}",
            "csrr {env}, henvcfg",
            "csrw henvcfg, {old}",
            "csrrsi {old}, hcounteren, {tm}",
            "csrr {counters}, hcounteren",
            "csrw hcounteren, {old}",
            ".option pop",
            old = out(reg) _,
            env = out(reg) env,
            counters = out(reg) counters,
            stce = in(reg) bits::HENVCFG_STCE,
            tm = const bits::COUNTER_TIME,
            options(nostack)
        );
    }
    env & bits::HENVCFG_STCE != 0 && counters & bits::COUNTER_TIME != 0
}

/// Probe this owner before unmasking STIE or acknowledging worker readiness.
/// # Safety
/// The caller owns this hart and timer with local interrupts masked, has an installed
/// native trap vector and logical owner in tp, and has initialized H capabilities.
/// The base H environment CSRs are accessible. This is a bootstrap operation:
/// the owner must not have an admitted vCPU or an active dispatcher.
#[cfg(target_arch = "riscv64")]
pub unsafe fn init_local_sstc() -> bool {
    let gates = available() && unsafe { probe_sstc_gates() };
    // A successful write proves HS compare access and disarms its reset value.
    // Firmware denial takes the exact native fixup before this owner is published.
    probe_owner_sstc(&SSTC_OWNERS, host::cpu_id(), gates, || unsafe {
        host::timer::probe_disarm_stimecmp()
    })
}

pub fn init_capabilities(hypervisor: bool, floating_point: bool, ssaia: bool) {
    PRESENT.store(hypervisor, Ordering::Relaxed);
    fp::init_capability(hypervisor && floating_point);
    aia::init_capability(hypervisor && ssaia);
}

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests;
