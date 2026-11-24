//! Pure virtual-device acceptance; never opens host devices or QEMU.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

use std::vec::Vec;
mod block;
mod console;
mod net;
mod rtc;

#[test]
fn board_console_vectors() {
    crate::acceptance::console();
}

#[test]
fn board_uart_vectors() {
    crate::acceptance::uart();
}
