//! VM-local bridges from pure frontends to the board's native device contracts.
//! Ownership/Lock Ordering: VM device owner -> link/native driver; never callback to VM.
//! Invariants: only copied host buffers enter native I/O; no guest DMA addresses.
pub mod block;
pub mod console;
pub mod net;
pub mod rtc;
