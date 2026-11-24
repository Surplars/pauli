//! Goldfish RTC register protocol, with VM-local wall-clock and alarm state.
//! The owner supplies monotonic nanoseconds; no host MMIO, ISA, allocation or
//! realtime source lives here. Changing UTC never changes the supplied clock.
//! Ownership/Lock Ordering: exclusive VM device owner; no locks or clock I/O.
//! Invariants: alarm countdown follows monotonic time, independent of UTC writes.
//! Alarm: disarmed -> armed -> pending -> acknowledged; cancel only disarms.
const TIME_LOW: usize = 0x00;
const TIME_HIGH: usize = 0x04;
const ALARM_LOW: usize = 0x08;
const ALARM_HIGH: usize = 0x0c;
const IRQ_ENABLED: usize = 0x10;
const CLEAR_ALARM: usize = 0x14;
const ALARM_STATUS: usize = 0x18;
const CLEAR_INTERRUPT: usize = 0x1c;
const IRQ_ENABLE: u32 = 1;

pub struct GoldfishRtc {
    monotonic_ns: u64,
    offset: u64,
    time_high: u32,
    alarm: u64,
    remaining_ns: u64,
    armed: bool,
    irq_enabled: bool,
    pending: bool,
    pub alarms: u64,
    pub time_writes: u64,
}

impl GoldfishRtc {
    pub const fn new(unix_ns: u64, monotonic_ns: u64) -> Self {
        Self {
            monotonic_ns,
            offset: unix_ns.wrapping_sub(monotonic_ns),
            time_high: 0,
            alarm: 0,
            remaining_ns: 0,
            armed: false,
            irq_enabled: false,
            pending: false,
            alarms: 0,
            time_writes: 0,
        }
    }
    pub fn unix_ns(&self) -> u64 {
        self.offset.wrapping_add(self.monotonic_ns)
    }

    pub fn advance(&mut self, monotonic_ns: u64) {
        let elapsed = monotonic_ns.wrapping_sub(self.monotonic_ns);
        self.monotonic_ns = monotonic_ns;
        if self.armed {
            if elapsed >= self.remaining_ns {
                self.armed = false;
                self.pending = true;
                self.alarms = self.alarms.wrapping_add(1);
            } else {
                self.remaining_ns -= elapsed;
            }
        }
    }

    pub fn irq_pending(&self) -> bool {
        self.pending && self.irq_enabled
    }

    /// Next armed IRQ in monotonic nanoseconds. A masked alarm still latches
    /// on advance; a deadline beyond the clock's range cannot wake the owner.
    pub fn irq_deadline_ns(&self) -> Option<u64> {
        if self.armed && self.irq_enabled {
            self.monotonic_ns.checked_add(self.remaining_ns)
        } else {
            None
        }
    }

    pub fn read_word(&mut self, offset: usize) -> Option<u32> {
        Some(match offset {
            TIME_LOW => {
                let now = self.unix_ns();
                self.time_high = (now >> 32) as u32;
                now as u32
            }
            TIME_HIGH => self.time_high,
            ALARM_LOW => self.alarm as u32,
            ALARM_HIGH => (self.alarm >> 32) as u32,
            IRQ_ENABLED => u32::from(self.irq_enabled),
            ALARM_STATUS => u32::from(self.armed),
            _ => return None,
        })
    }

    pub fn write_word(&mut self, offset: usize, value: u32) -> Option<()> {
        match offset {
            TIME_LOW | TIME_HIGH => {
                let now = self.unix_ns();
                let next = if offset == TIME_LOW {
                    (now & !0xffff_ffff) | u64::from(value)
                } else {
                    (u64::from(value) << 32) | (now & 0xffff_ffff)
                };
                self.offset = next.wrapping_sub(self.monotonic_ns);
                self.time_writes = self.time_writes.wrapping_add(1);
            }
            ALARM_LOW => {
                self.alarm = (self.alarm & !0xffff_ffff) | u64::from(value);
                // LOW commits the alarm. HIGH is just the programming latch;
                // later partial writes/UTC changes do not move an armed timer.
                self.remaining_ns = self.alarm.saturating_sub(self.unix_ns());
                self.armed = true;
                self.advance(self.monotonic_ns); // A past alarm expires immediately.
            }
            ALARM_HIGH => self.alarm = (u64::from(value) << 32) | (self.alarm & 0xffff_ffff),
            IRQ_ENABLED => self.irq_enabled = value & IRQ_ENABLE != 0,
            CLEAR_ALARM => self.armed = false, // Cancel does not acknowledge a latched IRQ.
            CLEAR_INTERRUPT => self.pending = false,
            _ => return None,
        }
        Some(())
    }
}
