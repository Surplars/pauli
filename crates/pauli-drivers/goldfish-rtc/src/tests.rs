//! RTC register-pair checks using private RAM, no host wall-clock mutation.
//! Ownership/Lock Ordering: local fixture; latch read is serialized by the driver.
//! Invariants: LOW is read before HIGH, and malformed windows are rejected.

use super::*;
struct FakeIrq;
// SAFETY: no actual hardware IRQ exists in this private fixture.
unsafe impl LocalIrq for FakeIrq {
    type Saved = ();
    fn save() {}
    unsafe fn restore(_: ()) {}
}
#[test]
fn native_time_pair() {
    let mut words = [0x1234u32, 0x5678];
    let resource = DeviceResource {
        base: words.as_mut_ptr() as usize,
        size: 8,
        irq: None,
    };
    // SAFETY: private aligned words outlive the native register reader.
    let rtc = unsafe { GoldfishRtc::<FakeIrq>::new(resource).unwrap() };
    assert_eq!(rtc.unix_ns(), Ok(0x5678_0000_1234));
    // SAFETY: short window is rejected before a register read.
    assert!(
        unsafe {
            GoldfishRtc::<FakeIrq>::new(DeviceResource {
                size: 4,
                ..resource
            })
        }
        .is_err()
    );
}
