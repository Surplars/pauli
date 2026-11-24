//! Observable guest lines feeding the core's per-vCPU wake mask. No hardware CSRs.
use crate::{aia::virtualized::VirtualAia, irq_backend::RiscvIrqChip, plic::VirtualPlic};
use pauli_core::arch::{IrqRegion::Legacy, VirtualIrqChip};

#[test]
fn one_shared_plic_claim_lowers_both_context_lines() {
    let mut chip = RiscvIrqChip::Legacy(VirtualPlic::with_contexts(2));
    for (offset, value) in [(40, 1), (0x2000, 1 << 10), (0x2080, 1 << 10)] {
        assert_eq!(chip.access(Legacy, offset, Some(value)), Some(0));
    }
    chip.set_level(10, true);
    assert!(chip.pending(0) && chip.pending(1));
    assert_eq!(chip.access(Legacy, 0x200004, None), Some(10));
    assert!(!chip.pending(0) && !chip.pending(1));
    assert_eq!(chip.access(Legacy, 0x201004, None), Some(0));
}

#[test]
fn draining_a_bound_imsic_inbox_exposes_the_next_remote_msi() {
    let mut chip = VirtualAia::new(2);
    let mut physical = chip.take_file(1);
    physical
        .indirect(0xc0, usize::MAX, (1 << 5) | (1 << 9), true)
        .unwrap();
    physical.indirect(0x70, usize::MAX, 1, true).unwrap();
    for id in [5, 9] {
        assert!(!chip.line(1));
        assert_eq!(chip.msi_access(4096, Some(id)), Some(0));
        assert!(!chip.line(0) && chip.line(1));
        physical.merge_pending(chip.take_pending(1));
        assert_eq!(physical.topei(true), (id << 16) | id);
    }
    chip.put_file(1, physical);
    assert!(!chip.line(0) && !chip.line(1));
}
