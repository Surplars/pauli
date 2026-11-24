//! Production algorithm acceptance, using private host fixtures.
//! Ownership/Lock Ordering: each test owns its state; no actual CSR access.
//! Invariants: retain the original protocol assertions and rejection cases.
pub mod state;
pub use state::Statistics;
#[cfg(target_arch = "riscv64")]
mod owner;
#[cfg(target_arch = "riscv64")]
pub use owner::Resident;
