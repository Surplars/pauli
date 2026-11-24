//! Atomic per-epoch fence work, with native IPIs supplied only by physical owners.
//! Ownership/Lock Ordering: senders publish atomics; single owner performs/ACKs fences.
//! Invariants: an instruction fence never substitutes for translation retirement.

pub mod fence;
pub use fence::{FenceBatch, FenceQueue, FenceTicket};
#[cfg(target_arch = "riscv64")]
mod mailbox;
#[cfg(all(feature = "selftest", any(test, target_arch = "riscv64")))]
mod samples;
#[cfg(target_arch = "riscv64")]
pub use mailbox::{VcpuMailbox, acknowledge_kick};
