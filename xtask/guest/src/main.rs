#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]
//! Static acceptance PID1 running only in Linux VU mode.
//! Ownership/Lock Ordering: one process and its checked syscall buffers.
//! Invariants: exit markers follow real Linux syscalls, not virtual firmware prints.
#[cfg(not(target_os = "none"))]
fn main() {}
#[cfg(target_os = "none")]
mod guest;
#[cfg(any(test, target_os = "none"))]
mod rtc_evidence;
