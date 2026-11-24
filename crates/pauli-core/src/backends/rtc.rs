//! VM-local RTC seeded once from a native wall clock, then monotonic ticks.
//! Ownership/Lock Ordering: VM devices; native RTC read only at provisioning/reset.
//! Invariants: UTC/alarm state is private and never changes host or peer time.
use pauli_devices::goldfish_rtc::GoldfishRtc;
const NS_PER_SEC: u64 = 1_000_000_000;

pub struct Rtc {
    pub device: GoldfishRtc,
    hz: u64,
}
impl Rtc {
    pub fn new(unix_ns: u64, now: u64, hz: u64) -> Self {
        Self {
            device: GoldfishRtc::new(unix_ns, ticks_to_ns(now, hz)),
            hz,
        }
    }
    pub fn advance(&mut self, now: u64) {
        self.device.advance(ticks_to_ns(now, self.hz));
    }
    /// First native tick whose sampled monotonic time reaches the alarm. Round
    /// the absolute deadline, not the remaining interval from a truncated tick.
    pub fn irq_deadline(&self) -> Option<u64> {
        let ns = self.device.irq_deadline_ns()?;
        let tick = (u128::from(ns) * u128::from(self.hz)).div_ceil(u128::from(NS_PER_SEC));
        u64::try_from(tick).ok()
    }
    pub fn access(&mut self, offset: usize, write: Option<usize>) -> Option<usize> {
        match write {
            Some(value) => self.device.write_word(offset, value as u32).map(|()| 0),
            None => self.device.read_word(offset).map(|value| value as usize),
        }
    }
}
pub fn ticks_to_ns(ticks: u64, hz: u64) -> u64 {
    let seconds = ticks / hz;
    seconds
        .saturating_mul(NS_PER_SEC)
        .saturating_add(((ticks % hz) as u128 * u128::from(NS_PER_SEC) / hz as u128) as u64)
}

#[cfg(test)]
mod tests {
    use super::Rtc;

    fn arm(rtc: &mut Rtc, unix_ns: u64) {
        rtc.access(0x0c, Some((unix_ns >> 32) as usize)).unwrap();
        rtc.access(0x08, Some(unix_ns as u32 as usize)).unwrap();
        rtc.access(0x10, Some(1)).unwrap();
    }

    #[test]
    fn alarm_compare_is_the_first_reaching_tick_on_a_fractional_clock() {
        // At tick 2 of a 3 Hz clock the RTC samples 666_666_666 ns. The
        // next tick is 333_333_334 ns away, not ceil(delta * 3 / 1e9) ticks.
        for (delta, tick) in [(333_333_333, 3), (333_333_334, 3), (333_333_335, 4)] {
            let mut rtc = Rtc::new(7, 2, 3);
            arm(&mut rtc, 7 + delta);
            assert_eq!(rtc.irq_deadline(), Some(tick));
            rtc.advance(tick - 1);
            assert!(!rtc.device.irq_pending());
            assert_eq!(rtc.irq_deadline(), Some(tick));
            rtc.advance(tick);
            assert!(rtc.device.irq_pending());
            assert_eq!(rtc.irq_deadline(), None);
        }
    }

    #[test]
    fn out_of_range_alarms_never_install_an_early_compare() {
        let mut rtc = Rtc::new(0, u64::MAX - 3, super::NS_PER_SEC);
        arm(&mut rtc, 3);
        assert_eq!(rtc.irq_deadline(), Some(u64::MAX));
        arm(&mut rtc, 4); // Monotonic nanoseconds cannot represent this instant.
        assert_eq!(rtc.irq_deadline(), None);
        rtc.advance(u64::MAX);
        assert!(!rtc.device.irq_pending());

        let mut fast = Rtc::new(0, 0, 2 * super::NS_PER_SEC);
        arm(&mut fast, u64::MAX); // Fits in nanoseconds but not in native ticks.
        assert_eq!(fast.irq_deadline(), None);
        fast.advance(u64::MAX);
        assert!(!fast.device.irq_pending());
    }
}
