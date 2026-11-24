//! Parse the fixture's two-CPU Linux IRQ table; never assume a Linux IRQ number.
//! The table must contain one RTC action. Timer/IPI counts are not device proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub irq: u32,
    pub counts: [u64; 2],
}

impl Snapshot {
    pub(crate) fn parse(data: &[u8]) -> Option<Self> {
        let mut rows = core::str::from_utf8(data).ok()?.lines();
        if !rows.next()?.split_ascii_whitespace().eq(["CPU0", "CPU1"]) {
            return None;
        }
        let mut found = None;
        for row in rows {
            let Some((irq, fields)) = row.split_once(':') else {
                continue;
            };
            let mut words = fields.split_ascii_whitespace();
            let (Some(a), Some(b)) = (words.next(), words.next()) else {
                continue;
            };
            // platform IRQ actions can use the driver name or the OF device name.
            let Some(action) = words.last() else { continue };
            if !matches!(action, "rtc" | "rtc0" | "goldfish_rtc" | "goldfish-rtc")
                && !action.ends_with(".rtc")
            {
                continue;
            }
            // A shared action line cannot attribute its per-CPU increments to
            // the RTC. Linux separates multiple action names with ", ".
            if found.is_some() || fields.contains(", ") {
                return None;
            }
            found = Some(Self {
                irq: irq.trim().parse().ok()?,
                counts: [a.parse().ok()?, b.parse().ok()?],
            });
        }
        found
    }

    pub(crate) fn delivered(self, after: Self, cpu: usize) -> bool {
        self.irq == after.irq
            && after.counts[cpu].checked_sub(self.counts[cpu]) == Some(1)
            && self.counts[cpu ^ 1] == after.counts[cpu ^ 1]
    }
}

/// Linux packs the count above the low flag byte. Only a single alarm is armed.
pub(crate) fn one_alarm_read(n: usize, bytes: [u8; size_of::<usize>()]) -> bool {
    const ALARM: usize = (1 << 8) | 0x80 | 0x20; // one event, RTC_IRQF | RTC_AF
    n == bytes.len() && usize::from_ne_bytes(bytes) == ALARM
}

/// Linux RV64 struct rtc_time, including the three trailing fields from rtc.h.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub(crate) struct Time {
    sec: i32,
    min: i32,
    hour: i32,
    day: i32,
    month: i32, // zero-based
    year: i32,  // since 1900
    unused: [i32; 3],
}

impl Time {
    // The kernel supplied a valid UTC date. Leave two seconds so programming
    // the alarm close to the next second never makes the target immediately due.
    pub(crate) fn alarm(mut self) -> Self {
        self.sec += 2;
        if self.sec >= 60 {
            self.sec -= 60;
            self.min += 1;
            if self.min == 60 {
                self.min = 0;
                self.hour += 1;
            }
        }
        if self.hour == 24 {
            self.hour = 0;
            self.day += 1;
            let y = self.year + 1900;
            let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
            let days = match self.month {
                1 => 28 + i32::from(leap),
                3 | 5 | 8 | 10 => 30,
                _ => 31,
            };
            if self.day > days {
                self.day = 1;
                self.month += 1;
                if self.month == 12 {
                    self.month = 0;
                    self.year += 1;
                }
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_unique_rtc_action_and_selected_cpu_counts_are_evidence() {
        let table = b" CPU0 CPU1\n 4: 9 9 RISC-V INTC riscv-timer\n 18: 7 11 APLIC-MSI 11 Level 101000.rtc\nIPI0: 20 30 something\n";
        let s = Snapshot::parse(table).unwrap();
        assert_eq!(
            s,
            Snapshot {
                irq: 18,
                counts: [7, 11]
            }
        );
        assert!(s.delivered(
            Snapshot {
                counts: [7, 12],
                ..s
            },
            1
        ));
        for bad in [[8, 12], [7, 11], [7, 2], [7, 13]] {
            assert!(!s.delivered(Snapshot { counts: bad, ..s }, 1));
        }
        let last = Snapshot {
            counts: [7, u64::MAX],
            ..s
        };
        assert!(!last.delivered(
            Snapshot {
                counts: [7, 0],
                ..s
            },
            1
        ));
        assert!(
            Snapshot {
                counts: [7, u64::MAX - 1],
                ..s
            }
            .delivered(last, 1)
        );
        let mut duplicate = table.to_vec();
        duplicate.extend_from_slice(b" 20: 1 0 PLIC 3 goldfish_rtc\n");
        assert!(Snapshot::parse(&duplicate).is_none());
        assert!(Snapshot::parse(b"CPU0 CPU1\n 18: 7 11 PLIC 11 virtio, 101000.rtc\n").is_none());
        assert!(Snapshot::parse(b"CPU1 CPU0\n 18: 7 11 APLIC 11 rtc\n").is_none());
    }
    #[test]
    fn rtc_read_is_one_alarm_not_a_batch_or_an_unrelated_event() {
        let n = size_of::<usize>();
        assert!(one_alarm_read(n, 0x1a0usize.to_ne_bytes()));
        assert!(!one_alarm_read(n - 1, 0x1a0usize.to_ne_bytes()));
        for event in [0xa0usize, 0x2a0, 0x180, 0x1b0] {
            assert!(!one_alarm_read(n, event.to_ne_bytes()));
        }
    }
    #[test]
    fn alarm_carries_at_midnight_months_and_centuries() {
        for (year, month, day, next) in [
            (2024, 1, 28, (2024, 1, 29)),
            (2100, 1, 28, (2100, 2, 1)),
            (2000, 1, 29, (2000, 2, 1)),
            (2026, 11, 31, (2027, 0, 1)),
        ] {
            let t = Time {
                sec: 59,
                min: 59,
                hour: 23,
                day,
                month,
                year: year - 1900,
                ..Time::default()
            }
            .alarm();
            assert_eq!((t.hour, t.min, t.sec), (0, 0, 1));
            assert_eq!((t.year + 1900, t.month, t.day), next);
        }
    }
}
