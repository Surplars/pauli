//! Move an RTC route with an alarm outstanding. PLIC routes a pending request;
//! APLIC has already converted it to an MSI owned by the original IMSIC file.
use super::aia::{config, top, write};
use crate::{
    aia::{registers, virtualized::VirtualAia},
    interrupt::VirtualInterruptController,
    plic::VirtualPlic,
};
use pauli_config::GuestBoardLayout;
use pauli_devices::goldfish_rtc::GoldfishRtc;

const SOURCE: u32 = GuestBoardLayout::DEFAULT.rtc_irq;
const CLEAR_INTERRUPT: usize = 0x1c;
const TARGET: usize = registers::GENMSI + SOURCE as usize * 4;

fn fire(rtc: &mut GoldfishRtc, at: u32) {
    // Goldfish commits ALARM on the LOW write, after the guest sets HIGH.
    rtc.write_word(0x0c, 0).unwrap();
    rtc.write_word(0x08, at).unwrap();
    rtc.write_word(0x10, 1).unwrap();
    rtc.advance(u64::from(at - 1));
    assert!(!rtc.irq_pending());
    rtc.advance(u64::from(at));
    assert!(rtc.irq_pending());
}

#[test]
fn plic_retargets_a_latched_rtc_alarm_before_claim() {
    let mut rtc = GoldfishRtc::new(0, 0);
    let mut p = VirtualPlic::with_contexts(2);
    p.write_word(SOURCE as usize * 4, 1).unwrap();
    p.write_word(0x2000, 1 << SOURCE).unwrap();

    for at in [10, 20] {
        fire(&mut rtc, at);
        p.set_level(SOURCE, rtc.irq_pending());
        if at == 10 {
            assert!(p.external_pending_for(0) && !p.external_pending_for(1));
            // No new device edge: this latched request becomes claimable on CPU1.
            p.write_word(0x2000, 0).unwrap();
            p.write_word(0x2080, 1 << SOURCE).unwrap();
        }
        assert!(!p.external_pending_for(0) && p.external_pending_for(1));
        assert_eq!(p.read_word(0x200004), Some(0));
        assert_eq!(p.read_word(0x201004), Some(SOURCE));
        rtc.write_word(CLEAR_INTERRUPT, 1).unwrap();
        p.set_level(SOURCE, rtc.irq_pending());
        p.write_word(0x201004, SOURCE).unwrap();
        assert!(!p.external_pending_for(0) && !p.external_pending_for(1));
    }
    assert_eq!((rtc.alarms, p.claims, p.completions), (2, 2, 2));
}

fn rtc_aplic() -> VirtualAia {
    let mut a = VirtualAia::new(2);
    for cpu in 0..2 {
        config(&mut a, cpu);
    }
    write(&mut a, SOURCE as usize * 4, 6); // Active-high level source.
    write(&mut a, TARGET, 37);
    write(&mut a, 0, 0x104);
    a
}

#[test]
fn retargeting_aplic_does_not_move_an_already_forwarded_rtc_msi() {
    let mut rtc = GoldfishRtc::new(0, 0);
    let mut a = rtc_aplic();
    write(&mut a, registers::SETIENUM, SOURCE as usize);
    for (at, cpu, id) in [(10, 0, 37), (20, 1, 53)] {
        fire(&mut rtc, at);
        a.set_level(SOURCE, rtc.irq_pending());
        if cpu == 0 {
            write(&mut a, TARGET, (1 << 18) | 53); // The first MSI is in file 0.
        }
        assert!(a.line(cpu) && !a.line(1 - cpu));
        assert_eq!(top(&mut a, cpu, true).0, id << 16 | id);
        rtc.write_word(CLEAR_INTERRUPT, 1).unwrap();
        a.set_level(SOURCE, rtc.irq_pending());
        write(&mut a, registers::SETIPNUM_LE, SOURCE as usize); // Linux level EOI.
        assert!(!a.line(0) && !a.line(1));
    }
    assert_eq!((rtc.alarms, a.msis(), a.claims), (2, 2, 2));
}

#[test]
fn masked_rtc_alarm_and_held_level_eoi_use_the_current_aplic_target() {
    let mut rtc = GoldfishRtc::new(0, 0);
    let mut a = rtc_aplic();
    fire(&mut rtc, 10);
    a.set_level(SOURCE, rtc.irq_pending());
    write(&mut a, TARGET, (1 << 18) | 53);
    assert_eq!(a.msis(), 0); // Still pending in APLIC; no IMSIC owns it yet.
    write(&mut a, registers::SETIENUM, SOURCE as usize);
    assert!(!a.line(0) && a.line(1));
    assert_eq!(top(&mut a, 1, true).0, (53 << 16) | 53);

    // The RTC is still asserted. Moving the route after claim emits nothing
    // on its own, but Linux's explicit level EOI must use the new destination.
    write(&mut a, TARGET, 37);
    assert!(!a.line(0) && !a.line(1));
    write(&mut a, registers::SETIPNUM_LE, SOURCE as usize);
    assert!(a.line(0) && !a.line(1));
    assert_eq!(top(&mut a, 0, true).0, (37 << 16) | 37);
    rtc.write_word(CLEAR_INTERRUPT, 1).unwrap();
    a.set_level(SOURCE, rtc.irq_pending());
    write(&mut a, registers::SETIPNUM_LE, SOURCE as usize);
    assert!(!a.line(0) && !a.line(1));
    assert_eq!((rtc.alarms, a.msis(), a.claims), (1, 2, 2));
}
