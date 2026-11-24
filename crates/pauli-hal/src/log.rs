//! Boot-installed console and allocation-free host log formatting.
//! Ownership/Lock Ordering: release all business locks before logging; console serializes I/O.
//! Invariants: installation is single-assignment; missing/failed console returns fmt::Error.

use crate::sync::Once;
use core::fmt::{self, Write};

pub trait Console: Send + Sync {
    fn write(&self, bytes: &[u8]) -> fmt::Result;
}
static CONSOLE: Once<&'static dyn Console> = Once::new();

pub fn install(console: &'static dyn Console) -> Result<(), &'static dyn Console> {
    CONSOLE.set(console)
}
struct Writer(&'static dyn Console);
impl Write for Writer {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0.write(text.as_bytes())
    }
}
pub fn print(arguments: fmt::Arguments<'_>) -> fmt::Result {
    Writer(*CONSOLE.get().ok_or(fmt::Error)?).write_fmt(arguments)
}
pub fn write_bytes(bytes: &[u8]) -> fmt::Result {
    CONSOLE.get().ok_or(fmt::Error)?.write(bytes)
}

#[macro_export]
macro_rules! hv_println {
    ($($arg:tt)*) => { { let _ = $crate::log::print(core::format_args!("{}\n", core::format_args!($($arg)*))); } };
}
#[macro_export]
macro_rules! hv_info {
    ($($arg:tt)*) => { $crate::hv_println!("[HV] {}", core::format_args!($($arg)*)) };
}
#[macro_export]
macro_rules! hv_error {
    ($($arg:tt)*) => { $crate::hv_println!("[HV] ERROR {}", core::format_args!($($arg)*)) };
}
