//! Production algorithm acceptance, using private host fixtures.
//! Ownership/Lock Ordering: each test owns its state; no actual CSR access.
//! Invariants: retain the original protocol assertions and rejection cases.
mod aia;
mod aia_bank;
mod block_irq;
mod file_state;
mod gstage;
mod imsic_cost;
mod mmio;
mod mmio_fixture;
mod pending_transitions;
mod plic_cost;
mod plic_priority;
mod remote;
mod resident;
mod rtc_affinity;
mod sbi;
mod timer;
mod vmid;
mod vmid_model;
