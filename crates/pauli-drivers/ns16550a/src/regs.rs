//! 16550 register offsets/bit masks, independent of physical register stride.
//! Ownership/Lock Ordering: immutable constants; no state or locks.
//! Invariants: DLAB is cleared before enabling RX interrupts.

pub const RBR: usize = 0;
pub const THR: usize = 0;
pub const IER: usize = 1;
pub const IIR: usize = 2;
pub const IIR_FIFO: u8 = 0xc0;
pub const FCR: usize = 2;
pub const LCR: usize = 3;
pub const MCR: usize = 4;
pub const LSR: usize = 5;
pub const MSR: usize = 6;
pub const LSR_DR: u8 = 1;
pub const LSR_THRE: u8 = 1 << 5;
pub const LSR_TEMT: u8 = 1 << 6;
pub const LSR_BAD_BYTE: u8 = 0x1c;
pub const IER_RX: u8 = 1;
pub const IER_TX: u8 = 2;
pub const IER_LINE: u8 = 4;
pub const LCR_DLAB: u8 = 0x80;
pub const LCR_8N1: u8 = 3;
pub const FCR_ENABLE: u8 = 1;
pub const FCR_RESET_RX: u8 = 2;
pub const FCR_RESET_TX: u8 = 4;
pub const MCR_DTR: u8 = 1;
pub const MCR_RTS: u8 = 2;
pub const MCR_OUT2: u8 = 8;
