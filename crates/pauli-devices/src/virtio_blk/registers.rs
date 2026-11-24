//! VirtIO block register negotiation and reset, retaining native drain ownership.
//! Ownership/Lock Ordering: exclusive caller ownership; no locks in this module.
//! Invariants: live rings cannot be reconfigured; reset retains staging until drain.

use super::*;

const REG_SIZE_MAX: usize = 0x108;
const REG_SEG_MAX: usize = 0x10c;
const REG_BLOCK_SIZE: usize = 0x114;

impl<B: AsMut<[u8]>> VirtioBlock<B> {
    pub fn read_word(&self, offset: usize) -> Option<u32> {
        if offset & 3 != 0 || offset >= REGISTER_END {
            return None;
        }
        Some(match offset {
            REG_MAGIC => MMIO_MAGIC,
            REG_VERSION => MMIO_VERSION,
            REG_DEVICE_ID => DEVICE_BLOCK,
            REG_VENDOR => DEVICE_VENDOR, // device-model identifier, not a PCI vendor ID
            REG_DEVICE_FEATURES => match self.device_features_sel {
                0 => self.features() as u32,
                1 => (self.features() >> 32) as u32,
                _ => 0,
            },
            REG_QUEUE_NUM_MAX if self.queue_sel == 0 => u32::from(MAX_QUEUE_SIZE),
            REG_QUEUE_READY if self.queue_sel == 0 => u32::from(self.ready),
            REG_INTERRUPT_STATUS => self.interrupts,
            REG_STATUS => self.status,
            REG_CONFIG_GENERATION => 0, // immutable capacity/configuration
            REG_CONFIG_LOW => self.sectors as u32,
            REG_CONFIG_HIGH => (self.sectors >> 32) as u32,
            REG_SIZE_MAX => MAX_SEGMENT_SIZE as u32,
            REG_SEG_MAX => MAX_SEGMENTS as u32,
            REG_BLOCK_SIZE => 512,
            _ => 0, // reserved/unsupported queue and configuration fields
        })
    }

    pub fn write_word(&mut self, offset: usize, value: u32) -> Option<()> {
        if offset & 3 != 0 || offset >= REGISTER_END {
            return None;
        }
        match offset {
            REG_DEVICE_FEATURES_SEL => self.device_features_sel = value,
            REG_DRIVER_FEATURES_SEL => self.driver_features_sel = value,
            REG_DRIVER_FEATURES => {
                if self.status & FEATURES_OK != 0 {
                    self.broken();
                } else if self.driver_features_sel < 2 {
                    let shift = self.driver_features_sel * 32;
                    self.driver_features = (self.driver_features & !(0xffff_ffffu64 << shift))
                        | (u64::from(value) << shift);
                } else if value != 0 {
                    self.unsupported_features = true;
                }
            }
            REG_QUEUE_SEL => self.queue_sel = value,
            REG_QUEUE_NUM | REG_DESC_LOW | REG_DESC_HIGH | REG_AVAIL_LOW | REG_AVAIL_HIGH
            | REG_USED_LOW | REG_USED_HIGH
                if self.queue_sel == 0 =>
            {
                if self.ready {
                    self.broken();
                } else {
                    match offset {
                        REG_QUEUE_NUM if value <= u32::from(MAX_QUEUE_SIZE) => {
                            self.queue_num = value as u16
                        }
                        REG_QUEUE_NUM => self.broken(),
                        REG_DESC_LOW => self.descriptors = set_low(self.descriptors, value),
                        REG_DESC_HIGH => self.descriptors = set_high(self.descriptors, value),
                        REG_AVAIL_LOW => self.available = set_low(self.available, value),
                        REG_AVAIL_HIGH => self.available = set_high(self.available, value),
                        REG_USED_LOW => self.used = set_low(self.used, value),
                        REG_USED_HIGH => self.used = set_high(self.used, value),
                        _ => {}
                    }
                }
            }
            REG_QUEUE_READY if self.queue_sel == 0 => match value {
                0 => {
                    self.ready = false;
                    self.notified = false;
                    self.discard();
                }
                1 if self.status & FEATURES_OK != 0
                    && self.queue_num.is_power_of_two()
                    && self.descriptors & 15 == 0
                    && self.available & 1 == 0
                    && self.used & 3 == 0 =>
                {
                    self.ready = true;
                    // Inspect once even without a first kick: avail_event is
                    // device-owned and need not start at zero in guest RAM.
                    self.notified = self.event_idx();
                }
                _ => self.broken(),
            },
            REG_QUEUE_NOTIFY if value == 0 && self.ready => {
                self.notified = true;
                self.notifications.kicks += 1;
            }
            REG_INTERRUPT_ACK => self.interrupts &= !(value & IRQ_MASK),
            REG_STATUS => self.write_status(value),
            _ => {} // read-only, reserved or nonexistent queue
        }
        Some(())
    }

    pub(super) fn write_status(&mut self, value: u32) {
        if value == 0 {
            let buffer = self.buffer.take().unwrap();
            *self = Self::init(self.sectors, self.writable, buffer);
            return;
        }
        let driver_bits = DRIVER_BITS;
        if value & !(driver_bits | NEEDS_RESET) != 0 || self.status & driver_bits & !value != 0 {
            self.broken();
            return;
        }
        let mut next = value & driver_bits;
        if next & FEATURES_OK != 0 && self.status & FEATURES_OK == 0 {
            if next & (ACKNOWLEDGE | DRIVER) != (ACKNOWLEDGE | DRIVER)
                || self.unsupported_features
                || self.driver_features & !self.features() != 0
                || self.driver_features & REQUIRED_FEATURES != REQUIRED_FEATURES
            {
                next &= !(FEATURES_OK | DRIVER_OK);
            }
        }
        if next & DRIVER_OK != 0
            && next & (ACKNOWLEDGE | DRIVER | FEATURES_OK) != (ACKNOWLEDGE | DRIVER | FEATURES_OK)
        {
            self.broken();
            return;
        }
        self.status = next | (self.status & NEEDS_RESET);
        if next & FAILED != 0 {
            self.discard();
        }
    }

    pub(super) fn broken(&mut self) {
        self.status |= NEEDS_RESET;
        self.notified = false;
        self.discard();
        if self.status & DRIVER_OK != 0 {
            self.interrupts |= IRQ_CONFIG;
        }
    }

    /// Forget guest destinations immediately, but retain host storage until
    /// every old backend slot drains. New queue admission cannot overtake it.
    pub(super) fn discard(&mut self) {
        self.available_armed = false;
        self.pending = [const { None }; MAX_INFLIGHT];
        self.inflight = 0;
        self.barrier = None;
        self.draining = true;
    }
}
