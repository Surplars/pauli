#![no_std]
#![forbid(unsafe_code)]
//! Pure guest device/console protocols. No host MMIO, CPU registers, IRQ numbers,
//! physical transport or kernel/architecture dependency. A board maps register
//! offsets and connects `irq_pending()` to its own controller/backend.
//! Ownership/Lock Ordering: serialized VM owner, then backend; no internal locks.
//! Invariants: bounds-checked copies only; physical DMA never targets guest RAM.
#[cfg(test)]
#[macro_use]
extern crate std;
#[cfg(any(test, feature = "acceptance"))]
pub mod acceptance;
pub mod console;
pub mod goldfish_rtc;
pub mod memory;
pub mod net_link;
#[cfg(test)]
mod tests;
pub mod uart16550;
mod virtio;
pub mod virtio_blk;
pub mod virtio_net;
