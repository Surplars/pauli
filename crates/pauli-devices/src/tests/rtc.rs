//! rtc protocol and isolation checks.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

//! Pure RTC protocol acceptance; no physical RTC, guest, disk or host clock writes.
use crate::goldfish_rtc::GoldfishRtc;

fn read(rtc: &mut GoldfishRtc) -> u64 {
    let low = rtc.read_word(0).unwrap();
    (u64::from(rtc.read_word(4).unwrap()) << 32) | u64::from(low)
}
fn write_time(rtc: &mut GoldfishRtc, time: u64) {
    rtc.write_word(4, (time >> 32) as u32).unwrap();
    rtc.write_word(0, time as u32).unwrap();
}
fn alarm(rtc: &mut GoldfishRtc, time: u64) {
    rtc.write_word(0xc, (time >> 32) as u32).unwrap();
    rtc.write_word(8, time as u32).unwrap();
}
#[test]
fn deadline_only_for_an_armed_unmasked_alarm() {
    let mut rtc = GoldfishRtc::new(0, 0);
    alarm(&mut rtc, 100);
    assert_eq!(rtc.irq_deadline_ns(), None);
    rtc.write_word(0x10, 1).unwrap();
    assert_eq!(rtc.irq_deadline_ns(), Some(100));
    rtc.advance(99);
    assert_eq!(rtc.irq_deadline_ns(), Some(100));
    rtc.advance(100);
    assert!(rtc.irq_pending());
    assert_eq!(rtc.irq_deadline_ns(), None);
    alarm(&mut rtc, 200);
    assert_eq!(rtc.irq_deadline_ns(), Some(200), "rearming is not an ACK");
    rtc.write_word(0x14, 1).unwrap();
    assert_eq!(rtc.irq_deadline_ns(), None);
    assert!(rtc.irq_pending(), "cancelling does not acknowledge the IRQ");
}

#[test]
fn acceptance() {
    let seed = 1_794_000_000_000_000_000u64;
    let mut rtc = GoldfishRtc::new(seed, 123);
    assert_eq!(read(&mut rtc), seed);
    rtc.advance(1_000_000_123);
    assert_eq!(read(&mut rtc), seed + 1_000_000_000);
    let mut rollover = GoldfishRtc::new(0xffff_fffe, 0);
    assert_eq!(rollover.read_word(0), Some(0xffff_fffe));
    rollover.advance(3);
    assert_eq!(
        rollover.read_word(4),
        Some(0),
        "HIGH remains latched across rollover"
    );
    assert_eq!(read(&mut rollover), 0x1_0000_0001);

    let mut other = GoldfishRtc::new(seed, 123);
    write_time(&mut rtc, 42_000_000_000);
    assert_eq!(read(&mut rtc), 42_000_000_000);
    rtc.advance(2_000_000_123);
    assert_eq!(read(&mut rtc), 43_000_000_000);
    assert_eq!(
        read(&mut other),
        seed,
        "VM writes never change another RTC/source"
    );

    alarm(&mut rtc, 45_000_000_000);
    assert_eq!(rtc.read_word(0x18), Some(1));
    rtc.advance(4_000_000_123);
    assert_eq!(rtc.read_word(0x18), Some(0));
    assert!(!rtc.irq_pending(), "expiry is latched while disabled");
    rtc.write_word(0x10, 1).unwrap();
    assert!(rtc.irq_pending());
    rtc.write_word(0x10, 0).unwrap();
    assert!(!rtc.irq_pending());
    rtc.write_word(0x10, 1).unwrap();
    assert!(rtc.irq_pending(), "masking does not acknowledge");
    rtc.write_word(0x14, 1).unwrap();
    assert!(
        rtc.irq_pending(),
        "alarm cancellation is not IRQ acknowledgement"
    );
    rtc.write_word(0x1c, 1).unwrap();
    assert!(!rtc.irq_pending());
    assert_eq!(rtc.alarms, 1);
    alarm(&mut rtc, 44_000_000_000);
    assert!(rtc.irq_pending(), "a past alarm expires immediately");
    rtc.write_word(0x1c, 1).unwrap();
    alarm(&mut rtc, 50_000_000_000);
    rtc.write_word(0x14, 1).unwrap();
    rtc.advance(20_000_000_123);
    assert!(!rtc.irq_pending(), "a cancelled alarm must stay cancelled");

    let mut staged = GoldfishRtc::new(0x1_0000_0000, 0);
    alarm(&mut staged, 0x1_0000_0100);
    staged.write_word(0xc, 0).unwrap(); // Do not reschedule until LOW commits.
    write_time(&mut staged, 0xffff_ffff_ffff_0000);
    staged.advance(255);
    assert!(!staged.irq_pending());
    staged.write_word(0x10, 1).unwrap();
    assert_eq!(staged.irq_deadline_ns(), Some(256), "alarm stays monotonic");
    staged.advance(256);
    assert!(
        staged.irq_pending(),
        "armed delay is independent of later UTC writes"
    );

    for offset in [1, 2, 0x14, 0x1c, 0x20, usize::MAX] {
        assert_eq!(rtc.read_word(offset), None);
    }
    for offset in [1, 2, 0x18, 0x20, usize::MAX] {
        assert_eq!(rtc.write_word(offset, 0), None);
    }
    println!(
        "RTC acceptance PASS: UTC progression, latched rollover, VM-local settime, alarm/mask/cancel/ack and invalid registers"
    );
}
