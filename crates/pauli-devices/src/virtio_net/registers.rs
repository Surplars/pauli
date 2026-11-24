//! VirtIO network register negotiation and queue setup; no packet I/O under MMIO.
//! Ownership/Lock Ordering: exclusive caller ownership; no locks in this module.
//! Invariants: invalid transitions break the device; reset clears rings, bounce and IRQ.

use super::*;

const CONFIG_MAC_LAST: usize = 5;
const CONFIG_STATUS_LOW: usize = 6;
const CONFIG_STATUS_HIGH: usize = 7;

impl VirtioNet {
    pub fn read_word(&self, offset: usize) -> Option<u32> {
        if offset & 3 != 0 || offset >= REGISTER_END {
            return None;
        }
        let q = self.queues.get(self.queue_sel as usize);
        Some(match offset {
            REG_MAGIC => MMIO_MAGIC,
            REG_VERSION => MMIO_VERSION,
            REG_DEVICE_ID => DEVICE_NET,
            REG_VENDOR => DEVICE_VENDOR,
            REG_DEVICE_FEATURES => match self.feature_sel {
                0 => FEATURES as u32,
                1 => (FEATURES >> 32) as u32,
                _ => 0,
            },
            REG_QUEUE_NUM_MAX => {
                if q.is_some() {
                    u32::from(QUEUE_MAX)
                } else {
                    0
                }
            }
            REG_QUEUE_READY => u32::from(q.is_some_and(|q| q.ready)),
            REG_INTERRUPT_STATUS => self.interrupts,
            REG_STATUS => self.status,
            REG_CONFIG_GENERATION => self.generation,
            REG_CONFIG_LOW => le32(&self.mac, 0),
            REG_CONFIG_HIGH => {
                u32::from(self.mac[4])
                    | (u32::from(self.mac[5]) << 8)
                    | (u32::from(self.link_up) << 16)
            }
            _ => 0,
        })
    }
    /// MAC/status config may be read bytewise by Linux. Transport is word-only.
    pub fn read_config_byte(&self, offset: usize) -> Option<u8> {
        match offset {
            0..=CONFIG_MAC_LAST => Some(self.mac[offset]),
            CONFIG_STATUS_LOW => Some(u8::from(self.link_up)),
            CONFIG_STATUS_HIGH => Some(0),
            _ => None,
        }
    }
    pub fn write_word(&mut self, offset: usize, value: u32) -> Option<()> {
        if offset & 3 != 0 || offset >= REGISTER_END {
            return None;
        }
        match offset {
            REG_DEVICE_FEATURES_SEL => self.feature_sel = value,
            REG_DRIVER_FEATURES_SEL => self.driver_sel = value,
            REG_DRIVER_FEATURES => {
                if self.status & FEATURES_OK != 0 {
                    self.broken();
                } else if self.driver_sel < 2 {
                    let shift = self.driver_sel * 32;
                    self.driver_features = (self.driver_features & !(0xffff_ffffu64 << shift))
                        | (u64::from(value) << shift);
                } else if value != 0 {
                    self.unsupported = true;
                }
            }
            REG_QUEUE_SEL => self.queue_sel = value,
            REG_INTERRUPT_ACK => self.interrupts &= !(value & IRQ_MASK),
            REG_STATUS => {
                self.write_status(value);
            }
            REG_QUEUE_NOTIFY => {} // Polling service also handles RX without a fresh notify.
            _ => {
                self.write_queue(offset, value);
            }
        }
        Some(())
    }

    fn write_status(&mut self, value: u32) {
        if value == 0 {
            let up = self.link_up;
            *self = Self::new(self.mac);
            self.link_up = up;
            return;
        }
        if value & !(DRIVER_BITS | NEEDS_RESET) != 0 || self.status & DRIVER_BITS & !value != 0 {
            self.broken();
            return;
        }
        let mut next = value & DRIVER_BITS;
        if next & FEATURES_OK != 0
            && self.status & FEATURES_OK == 0
            && (next & (ACKNOWLEDGE | DRIVER) != (ACKNOWLEDGE | DRIVER)
                || self.unsupported
                || self.driver_features & !FEATURES != 0
                || self.driver_features & VERSION_1 == 0)
        {
            next &= !OK;
        }
        if next & DRIVER_OK != 0
            && next & (ACKNOWLEDGE | DRIVER | FEATURES_OK) != (ACKNOWLEDGE | DRIVER | FEATURES_OK)
        {
            self.broken();
        } else {
            self.status = next | (self.status & BROKEN);
        }
    }

    fn write_queue(&mut self, offset: usize, value: u32) {
        if let Some(q) = self.queues.get_mut(self.queue_sel as usize) {
            match offset {
                REG_QUEUE_READY => match value {
                    0 => q.ready = false,
                    1 if self.status & FEATURES_OK != 0
                        && q.num.is_power_of_two()
                        && q.desc & 15 == 0
                        && q.avail & 1 == 0
                        && q.used & 3 == 0 =>
                    {
                        q.ready = true
                    }
                    _ => self.broken(),
                },
                REG_QUEUE_NUM | REG_DESC_LOW | REG_DESC_HIGH | REG_AVAIL_LOW | REG_AVAIL_HIGH
                | REG_USED_LOW | REG_USED_HIGH => {
                    if q.ready {
                        self.broken();
                    } else {
                        match offset {
                            REG_QUEUE_NUM if value <= u32::from(QUEUE_MAX) => q.num = value as u16,
                            REG_QUEUE_NUM => self.broken(),
                            REG_DESC_LOW => q.desc = low(q.desc, value),
                            REG_DESC_HIGH => q.desc = high(q.desc, value),
                            REG_AVAIL_LOW => q.avail = low(q.avail, value),
                            REG_AVAIL_HIGH => q.avail = high(q.avail, value),
                            REG_USED_LOW => q.used = low(q.used, value),
                            REG_USED_HIGH => q.used = high(q.used, value),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }
}
