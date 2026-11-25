//! A self-only TAKE drops the source before Linux completes a PLIC or APLIC IRQ.
use super::aia::{top, write};
use crate::{
    aia::{registers, virtualized::VirtualAia},
    interrupt::VirtualInterruptController,
    plic::VirtualPlic,
};
use pauli_config::{GuestBoardLayout, ImsicIds};

const EVENT: u32 = GuestBoardLayout::DEFAULT.control_irq;

#[test]
fn plic_control_claim_follows_affinity_and_one_already_latched_claim_may_be_empty() {
    let mut chip = VirtualPlic::with_contexts(2);
    chip.write_word(EVENT as usize * 4, 2).unwrap();
    chip.write_word(0x2080, 1 << EVENT).unwrap();
    chip.set_level(EVENT, true);
    assert!(!chip.external_pending_for(0) && chip.external_pending_for(1));
    assert_eq!(chip.read_word(0x201004), Some(EVENT));
    chip.set_level(EVENT, false); // TAKE before PLIC complete.
    chip.write_word(0x201004, EVENT).unwrap();
    assert!(!chip.external_pending_for(1));
    chip.set_level(EVENT, true);
    chip.set_level(EVENT, false); // Polling TAKE raced delivery.
    chip.write_word(0x2080, 0).unwrap();
    chip.write_word(0x2000, 1 << EVENT).unwrap();
    assert!(chip.external_pending_for(0) && !chip.external_pending_for(1));
    assert_eq!(chip.read_word(0x200004), Some(EVENT));
    chip.write_word(0x200004, EVENT).unwrap();
    assert_eq!(chip.read_word(0x200004), Some(0));
}

#[test]
fn control_msi_after_file_load_survives_put_and_follows_new_target_only_on_reassert() {
    for ids in [ImsicIds::Ids127, ImsicIds::Ids255] {
        let mut chip = VirtualAia::with_ids(2, ids);
        for cpu in 0..2 {
            // Program only an implemented EIE word on either file geometry.
            chip.csr(
                cpu,
                crate::aia::emulation::CsrAccess {
                    register: registers::SIREG,
                    select: registers::EIE,
                    clear: 0,
                    set: (1 << 37) | (1 << 53),
                    write: true,
                },
            )
            .unwrap();
            chip.csr(
                cpu,
                crate::aia::emulation::CsrAccess {
                    register: registers::SIREG,
                    select: registers::EIDELIVERY,
                    clear: 0,
                    set: 1,
                    write: true,
                },
            )
            .unwrap();
        }
        let target = registers::GENMSI + EVENT as usize * 4;
        write(&mut chip, EVENT as usize * 4, 6);
        write(&mut chip, target, (1 << 18) | 37);
        write(&mut chip, registers::SETIENUM, EVENT as usize);
        write(&mut chip, 0, 0x104);
        let saved = chip.take_file(1);
        chip.set_level(EVENT, true);
        chip.set_level(EVENT, true);
        assert!(chip.line(1) && !chip.line(0));
        write(&mut chip, target, 53);
        chip.set_level(EVENT, false); // Polling TAKE before the bound file is put.
        chip.put_file(1, saved);
        assert_eq!(top(&mut chip, 1, true).0, (37 << 16) | 37);
        write(&mut chip, registers::SETIPNUM_LE, EVENT as usize);
        assert!(!chip.line(1) && !chip.line(0));
        chip.set_level(EVENT, true);
        assert!(chip.line(0) && !chip.line(1));
        assert_eq!(top(&mut chip, 0, true).0, (53 << 16) | 53);
        chip.set_level(EVENT, false);
        write(&mut chip, registers::SETIPNUM_LE, EVENT as usize);
        assert_eq!(chip.msis(), 2);
        assert!(!chip.line(0) && !chip.line(1));
    }
}
