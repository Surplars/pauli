//! Shared VirtIO-MMIO v2 register offsets and status/interrupt encodings.
//! Ownership/Lock Ordering: immutable protocol constants; no state or locks.
//! Invariants: values are guest ABI offsets, never physical MMIO addresses.

pub(crate) const REG_MAGIC: usize = 0x00;
pub(crate) const REG_VERSION: usize = 0x04;
pub(crate) const REG_DEVICE_ID: usize = 0x08;
pub(crate) const REG_VENDOR: usize = 0x0c;
pub(crate) const REG_DEVICE_FEATURES: usize = 0x10;
pub(crate) const REG_DEVICE_FEATURES_SEL: usize = 0x14;
pub(crate) const REG_DRIVER_FEATURES: usize = 0x20;
pub(crate) const REG_DRIVER_FEATURES_SEL: usize = 0x24;
pub(crate) const REG_QUEUE_SEL: usize = 0x30;
pub(crate) const REG_QUEUE_NUM_MAX: usize = 0x34;
pub(crate) const REG_QUEUE_NUM: usize = 0x38;
pub(crate) const REG_QUEUE_READY: usize = 0x44;
pub(crate) const REG_QUEUE_NOTIFY: usize = 0x50;
pub(crate) const REG_INTERRUPT_STATUS: usize = 0x60;
pub(crate) const REG_INTERRUPT_ACK: usize = 0x64;
pub(crate) const REG_STATUS: usize = 0x70;
pub(crate) const REG_DESC_LOW: usize = 0x80;
pub(crate) const REG_DESC_HIGH: usize = 0x84;
pub(crate) const REG_AVAIL_LOW: usize = 0x90;
pub(crate) const REG_AVAIL_HIGH: usize = 0x94;
pub(crate) const REG_USED_LOW: usize = 0xa0;
pub(crate) const REG_USED_HIGH: usize = 0xa4;
pub(crate) const REG_CONFIG_GENERATION: usize = 0xfc;
pub(crate) const REG_CONFIG_LOW: usize = 0x100;
pub(crate) const REG_CONFIG_HIGH: usize = 0x104;
pub(crate) const REGISTER_END: usize = 0x200;
pub(crate) const MMIO_MAGIC: u32 = 0x7472_6976;
pub(crate) const MMIO_VERSION: u32 = 2;
pub(crate) const DEVICE_VENDOR: u32 = 0x4f49_4f4e;
pub(crate) const DEVICE_NET: u32 = 1;
pub(crate) const DEVICE_BLOCK: u32 = 2;

pub(crate) const ACKNOWLEDGE: u32 = 1;
pub(crate) const DRIVER: u32 = 2;
pub(crate) const DRIVER_OK: u32 = 4;
pub(crate) const FEATURES_OK: u32 = 8;
pub(crate) const NEEDS_RESET: u32 = 64;
pub(crate) const FAILED: u32 = 128;
pub(crate) const DRIVER_BITS: u32 = ACKNOWLEDGE | DRIVER | DRIVER_OK | FEATURES_OK | FAILED;
pub(crate) const IRQ_USED: u32 = 1;
pub(crate) const IRQ_CONFIG: u32 = 2;
pub(crate) const IRQ_MASK: u32 = IRQ_USED | IRQ_CONFIG;
pub(crate) const VERSION_1: u64 = 1 << 32;

/// Fixed offsets into a local snapshot, never an unchecked guest-provided index.
pub(crate) fn le16(bytes: &[u8], offset: usize) -> u16 {
    let mut word = [0; 2];
    word.copy_from_slice(&bytes[offset..offset + 2]);
    u16::from_le_bytes(word)
}

pub(crate) fn le32(bytes: &[u8], offset: usize) -> u32 {
    let mut word = [0; 4];
    word.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(word)
}

pub(crate) fn le64(bytes: &[u8], offset: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(word)
}
