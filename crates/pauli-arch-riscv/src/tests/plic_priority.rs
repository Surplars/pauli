//! Check line notification and destructive claim against the guest-visible priorities.
use crate::{interrupt::VirtualInterruptController, plic::VirtualPlic};

const ENABLE: usize = 0x2000;
const PENDING: usize = 0x1000;
const THRESHOLD: usize = 0x200000;
const CLAIM: usize = THRESHOLD + 4;
const HIGH: u32 = 31;
const BOTH: u32 = (1 << HIGH) | (1 << 1);

#[test]
fn threshold_and_claim_are_distinct_for_every_priority_pair() {
    for lo in 0..8 {
        for hi in 0..8 {
            for threshold in 0..8 {
                let mut p = VirtualPlic::new();
                p.write_word(0, 7).unwrap();
                p.write_word(4, lo | !7).unwrap();
                p.write_word(HIGH as usize * 4, hi | !7).unwrap();
                p.write_word(ENABLE, BOTH | 1).unwrap();
                p.write_word(THRESHOLD, threshold).unwrap();
                for id in [0, 1, HIGH] {
                    p.set_level(id, true);
                }
                assert_eq!(p.read_word(0), Some(0));
                assert_eq!(p.read_word(4), Some(lo));
                assert_eq!(p.read_word(HIGH as usize * 4), Some(hi));
                assert_eq!(p.read_word(ENABLE), Some(BOTH));
                assert_eq!(p.read_word(PENDING), Some(BOTH));
                assert_eq!(p.external_pending(), lo.max(hi) > threshold);
                assert_eq!(p.source_pending_for(0, HIGH), hi > threshold);

                // Claim ignores the threshold, skips zero priority and breaks ties by ID.
                let winner = if hi > lo { HIGH } else { u32::from(lo != 0) };
                assert_eq!(p.read_word(CLAIM), Some(winner));
                assert_eq!(p.external_pending(), lo.min(hi) > threshold);
                let runner_up = if lo.min(hi) != 0 {
                    1 + HIGH - winner
                } else {
                    0
                };
                assert_eq!(p.read_word(CLAIM), Some(runner_up));
                assert_eq!(p.read_word(CLAIM), Some(0));
            }
        }
    }
}

#[test]
fn priority_rewrites_do_not_create_or_cancel_gateway_requests() {
    let mut p = VirtualPlic::new();
    let register = HIGH as usize * 4;
    p.write_word(ENABLE, 1 << HIGH).unwrap();
    p.write_word(THRESHOLD, 4).unwrap();
    p.set_level(HIGH, true);
    for priority in [0, 7, 4, 5] {
        p.write_word(register, priority).unwrap();
        assert_eq!(p.read_word(PENDING), Some(1 << HIGH));
        assert_eq!(p.external_pending(), priority > 4);
    }
    assert_eq!(p.read_word(CLAIM), Some(HIGH));
    p.write_word(register, 0).unwrap();
    assert_eq!(p.read_word(PENDING), Some(0)); // Still in service.
    p.write_word(CLAIM, HIGH).unwrap(); // Held level re-pends, even at zero priority.
    assert_eq!(p.read_word(PENDING), Some(1 << HIGH));
    assert!(!p.external_pending());
    p.write_word(register, 7).unwrap();
    assert!(p.external_pending());

    p.retire_source(HIGH);
    // A retired and a new source both start disabled with zero priority.
    for chip in [&mut p, &mut VirtualPlic::new()] {
        assert_eq!(chip.read_word(register), Some(0));
        assert_eq!(chip.read_word(PENDING), Some(0));
        assert_eq!(chip.read_word(ENABLE), Some(0));
        chip.write_word(ENABLE, 1 << HIGH).unwrap();
        chip.set_level(HIGH, true);
        assert!(!chip.external_pending());
        assert_eq!(chip.read_word(CLAIM), Some(0));
    }
}
