//! Exercise the public IRQ constructor in an isolated host process, including
//! the boot-time Ssaia discovery setting shared by all advanced guests.
use pauli_arch_riscv::{aia, irq_backend::RiscvIrqChip};
use pauli_config::{GuestBoardLayout, GuestIrqConfig, ImsicIds, MAX_GUEST_VCPUS};
use pauli_core::{
    HvError,
    arch::{IrqRegion, VirtualIrqChip},
};

#[test]
fn legacy_configuration_limits_contexts_even_without_ssaia() {
    let irq = GuestBoardLayout::DEFAULT.irq_config(2, false).unwrap();
    let mut chip = RiscvIrqChip::create_configured(irq).unwrap();
    assert_eq!(chip.access(IrqRegion::Legacy, 0x20_1004, None), Some(0));
    assert_eq!(chip.access(IrqRegion::Legacy, 0x20_2004, None), None);
    assert_eq!(chip.access(IrqRegion::Message, 0, Some(1)), None);
    for vcpus in [0, MAX_GUEST_VCPUS + 1] {
        assert!(matches!(
            RiscvIrqChip::create_configured(GuestIrqConfig { vcpus, ..irq }),
            Err(HvError::InvalidVcpu)
        ));
    }
}

#[test]
fn advanced_configuration_requires_ssaia_and_preserves_the_selected_file_geometry() {
    for discovered in [false, true] {
        aia::init_capability(discovered);
        for ids in [ImsicIds::Ids127, ImsicIds::Ids255] {
            let layout = GuestBoardLayout {
                imsic_ids: ids,
                ..GuestBoardLayout::DEFAULT
            };
            let irq = layout.irq_config(2, true).unwrap();
            let chip = RiscvIrqChip::create_configured(irq);
            if !(discovered && cfg!(feature = "aia")) {
                assert!(matches!(chip, Err(HvError::Unsupported)));
                continue;
            }
            let RiscvIrqChip::Advanced(mut aia) = chip.unwrap() else {
                panic!("expected the configured APLIC and IMSIC");
            };
            assert_eq!(aia.ids(), ids);
            assert_eq!(aia.msi_access(4096, Some(ids.max_id())), Some(0));
            assert_eq!(aia.msi_access(4096, Some(ids.max_id() + 1)), Some(0));
            assert_eq!(aia.msi_access(8192, Some(1)), None);
            let mut file = aia.take_file(1);
            let mut expected = [0; aia::imsic::WORDS];
            expected[ids.max_id() / 64] = 1 << (ids.max_id() % 64);
            assert_eq!(file.take_pending(), expected);
            aia.put_file(1, file);
        }
    }
}
