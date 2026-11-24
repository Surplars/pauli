#![no_std]
//! Minimal host services; no scheduler, filesystem, allocator or ISA dependency.
//! Ownership/Lock Ordering: VM/vCPU -> devices -> host drivers; never log under a lock.
//! Invariants: board-validated resources and exclusive DMA storage cross the HAL.

pub mod addr;
pub mod device;
pub mod dma;
pub mod fdt;
pub mod irq;
pub mod log;
pub mod mem;
pub mod mmio;
pub mod sync;

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests;
