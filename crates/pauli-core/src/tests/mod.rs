//! Core host tests, with explicit private resources and no native ISA calls.
//! Ownership/Lock Ordering: each test owns fixtures; production protocols are reused.
//! Invariants: boundary rejection is side-effect free and state transitions are bounded.
mod capacity;
mod console_rx;
#[cfg(feature = "guest-control")]
mod control_events;
mod direct_console;
mod dispatch_irq;
mod fence_report;
mod firmware_children;
mod foundations;
mod idle;
mod io_clock;
mod management;
mod model;
mod model_cpu;
mod placement;
mod provision;
mod quota;
mod restart;
mod runtime;
mod shape;
mod sstc;
mod storage;
mod uart_capture;
mod uncontended;
mod volumes;
mod wake_delivery;
mod wide;
mod workers;
