//! VMID policy plus physical-owner bindings; tag zero always belongs to HS.
//! Ownership/Lock Ordering: one IRQ-masked owner per cache; no global lock.
//! Invariants: reuse invalidates old tags, and retirement covers every visited CPU.

pub mod policy;
pub use policy::Statistics;
#[cfg(target_arch = "riscv64")]
pub(crate) use policy::TAGGED;
#[cfg(target_arch = "riscv64")]
mod owner;
#[cfg(target_arch = "riscv64")]
pub(crate) use owner::{prepare, retire, synchronize};

/// The loader's publication orders image stores, but only the executing hart
/// can synchronize its instruction fetch. Commit the mask after successful entry:
/// a failed prepare has no translations to retire and must not mark the hart warm.
#[cfg(any(target_arch = "riscv64", test))]
pub(crate) fn entering_hart(residents: usize, hart: usize, sync_fetch: impl FnOnce()) -> usize {
    let bit = 1usize << hart;
    if residents & bit == 0 {
        sync_fetch();
    }
    residents | bit
}
