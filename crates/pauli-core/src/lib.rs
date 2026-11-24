#![no_std]
//! Architecture-neutral VMM ownership, scheduling, management and device bridges.
//! Ownership/Lock Ordering: manager -> vCPU metadata -> devices -> links/drivers.
//! Invariants: no lock spans guest execution; RAM reuse follows CPU and I/O join.
pub mod arch;
pub mod backends;
pub mod board;
mod boot;
pub mod bus;
pub mod manager;
pub mod memory;
pub mod monitor;
mod placement;
pub mod runtime;
pub mod trace;
pub mod vm;
pub use boot::{BootStorage, boot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HvError {
    Unsupported,
    InvalidConfig,
    InvalidMemory,
    InvalidImage,
    InvalidState,
    InvalidVcpu,
    InvalidDevice,
    InvalidCommand,
    NotFound,
    Busy,
    Exhausted,
    Permission,
    Timeout,
    Fault,
}
pub type Result<T> = core::result::Result<T, HvError>;
impl From<pauli_hal::device::DeviceError> for HvError {
    fn from(error: pauli_hal::device::DeviceError) -> Self {
        use pauli_hal::device::DeviceError as E;
        match error {
            E::Busy | E::NotReady => Self::Busy,
            E::NotSupported => Self::Unsupported,
            E::Exhausted => Self::Exhausted,
            E::Timeout => Self::Timeout,
            E::ReadOnly => Self::Permission,
            E::InvalidResource | E::Stale => Self::InvalidDevice,
            E::Fault => Self::Fault,
        }
    }
}
#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests;
