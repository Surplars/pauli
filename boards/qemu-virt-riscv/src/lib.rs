#![no_std]
//! QEMU virt firmware discovery shared by the native board and offline tests.
//! Ownership/Lock Ordering: immutable OpenSBI input, before publication to workers.
//! Invariants: only the binary programs devices; this library only resolves resources.
pub mod fdt;
pub mod guest_tree;
#[cfg(test)]
extern crate std;
