//! Native transport register offsets, not board addresses.
//! Ownership/Lock Ordering: immutable constants, no locks.
//! Invariants: every fixed register is aligned and fits MIN_BYTES.

pub const MAGIC_VALUE: u32 = 0x7472_6976;
pub const MODERN_VERSION: u32 = 2;
pub const MAGIC: usize = 0x000;
pub const VERSION: usize = 0x004;
pub const DEVICE_ID: usize = 0x008;
pub const DEVICE_FEATURES: usize = 0x010;
pub const DEVICE_FEATURES_SEL: usize = 0x014;
pub const DRIVER_FEATURES: usize = 0x020;
pub const DRIVER_FEATURES_SEL: usize = 0x024;
pub const QUEUE_SEL: usize = 0x030;
pub const QUEUE_NUM_MAX: usize = 0x034;
pub const QUEUE_NUM: usize = 0x038;
pub const QUEUE_READY: usize = 0x044;
pub const QUEUE_NOTIFY: usize = 0x050;
pub const INTERRUPT_STATUS: usize = 0x060;
pub const INTERRUPT_ACK: usize = 0x064;
pub const STATUS: usize = 0x070;
pub const QUEUE_DESC_LOW: usize = 0x080;
pub const QUEUE_DRIVER_LOW: usize = 0x090;
pub const QUEUE_DEVICE_LOW: usize = 0x0a0;
pub const CONFIG: usize = 0x100;
pub const MIN_BYTES: usize = 0x200;
