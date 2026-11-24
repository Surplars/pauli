//! Virtual interrupt-controller protocol level, not host IRQ routing.
//! Ownership/Lock Ordering: vCPU/device owner serializes updates, no lock here.
//! Invariants: wired source IDs never stand for physical interrupt numbers.

pub trait VirtualInterruptController {
    fn set_level(&mut self, source: u32, asserted: bool);
    fn external_pending(&self) -> bool;
}
