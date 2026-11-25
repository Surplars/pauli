//! Default virtual board ABI and mutually exclusive interrupt topologies.
//! Ownership/Lock Ordering: immutable local values; no locks or host resources.
//! Invariants: peripheral regions are disjoint; APLIC replaces, not augments, PLIC.

use super::*;
use crate::{
    GuestBoardLayout, GuestIrqConfig, GuestIrqController, ImsicIds, MAX_GUEST_VCPUS, MmioRegion,
    overlaps,
};

#[test]
fn default_board_abi() {
    let board = GuestBoardLayout::default();
    assert_eq!(board, GuestBoardLayout::DEFAULT);
    assert_eq!(VmConfig::new("guest").unwrap().guest_base, board.ram_base);
    assert_eq!(board.ram_base, 0x8000_0000);
    for (region, base, size) in [
        (board.uart, 0x1000_0000, 0x100),
        (board.block, 0x1000_1000, 0x1000),
        (board.net, 0x1000_2000, 0x1000),
        (board.rtc, 0x0010_1000, 0x1000),
        (board.plic, 0x0c00_0000, 0x0021_0000),
        (board.aplic, 0x0c00_0000, 0x4000),
        (board.imsic, 0x2800_0000, 0x1_0000),
    ] {
        assert_eq!(region, MmioRegion { base, size });
    }
    assert_eq!(
        [
            board.block_irq,
            board.net_irq,
            board.uart_irq,
            board.rtc_irq
        ],
        [1, 2, 10, 11]
    );
    assert_eq!(board.imsic.size, u64::from(MAX_GUEST_VCPUS) * 4096);
    assert_eq!(board.control_irq, 12);
    for source in [
        0,
        32,
        board.uart_irq,
        board.block_irq,
        board.net_irq,
        board.rtc_irq,
    ] {
        let invalid = GuestBoardLayout {
            control_irq: source,
            ..board
        };
        for advanced in [false, true] {
            assert_eq!(invalid.irq_config(1, advanced), Err(Error::Dtb));
        }
    }
}

#[test]
fn interrupt_apertures_stop_at_the_guest_topology() {
    let mut board = GuestBoardLayout::DEFAULT;
    assert_eq!(board.imsic_files(1).unwrap().size, 4096);
    assert_eq!(board.imsic_files(16).unwrap(), board.imsic);
    // Even one missing byte prevents publishing an unusable final file/register.
    board.imsic.size -= 1;
    assert!(board.imsic_files(16).is_none());
    assert_eq!(board.imsic_files(15).unwrap().size, 15 * 4096);
    board.plic.size = 0x20_f008;
    assert_eq!(board.plic_contexts(16), Some(board.plic));
    board.plic.size -= 1;
    assert_eq!(board.plic_contexts(16), None);
    assert_eq!(board.plic_contexts(15), Some(board.plic));
}

#[test]
fn irq_config_selects_board_apertures_and_only_the_vms_files() {
    let mut board = GuestBoardLayout::DEFAULT;
    board.plic.base = 0x0900_0000;
    board.aplic.base = 0x0a00_0000;
    board.imsic.base = 0x2500_0000;
    for vcpus in [1, 3, MAX_GUEST_VCPUS] {
        assert_eq!(
            board.irq_config(vcpus, false),
            Ok(GuestIrqConfig {
                vcpus,
                controller: GuestIrqController::Legacy { region: board.plic }
            })
        );
        for ids in [ImsicIds::Ids127, ImsicIds::Ids255] {
            board.imsic_ids = ids;
            assert_eq!(
                board.irq_config(vcpus, true),
                Ok(GuestIrqConfig {
                    vcpus,
                    controller: GuestIrqController::Advanced {
                        wired: board.aplic,
                        message: MmioRegion {
                            base: board.imsic.base,
                            size: u64::from(vcpus) * 4096
                        },
                        ids,
                    }
                })
            );
        }
    }
}

#[test]
fn irq_config_rejects_invalid_counts_and_selected_windows() {
    let layout = GuestBoardLayout::DEFAULT;
    for vcpus in [0, MAX_GUEST_VCPUS + 1, u8::MAX] {
        for advanced in [false, true] {
            assert_eq!(layout.irq_config(vcpus, advanced), Err(Error::Vcpus));
        }
    }
    let mut board = layout;
    board.plic.size = 0x20_1007; // Last byte of context 1's claim register is missing.
    assert_eq!(board.irq_config(2, false), Err(Error::Dtb));
    assert!(board.irq_config(1, false).is_ok());
    assert!(board.irq_config(2, true).is_ok());
    for base in [1, u64::MAX - 3] {
        board.plic.base = base;
        assert_eq!(board.irq_config(1, false), Err(Error::Dtb));
    }

    board = layout;
    board.imsic.size = 2 * 4096 - 1;
    assert_eq!(board.irq_config(2, true), Err(Error::Dtb));
    assert!(board.irq_config(1, true).is_ok());
    assert!(board.irq_config(2, false).is_ok());
    for base in [1, u64::MAX - 4095, board.aplic.base] {
        board.imsic.base = base;
        assert_eq!(board.irq_config(1, true), Err(Error::Dtb));
    }
    board = layout;
    board.aplic.size -= 1;
    assert_eq!(board.irq_config(1, true), Err(Error::Dtb));
    assert!(board.irq_config(1, false).is_ok());
    board = layout;
    board.aplic.base = board.imsic.base + 4096;
    assert!(board.irq_config(1, true).is_ok());
    assert_eq!(board.irq_config(2, true), Err(Error::Dtb));
}

#[test]
fn peripheral_regions_are_disjoint() {
    let board = GuestBoardLayout::DEFAULT;
    for interrupt in [board.plic, board.aplic] {
        let regions = [
            board.uart,
            board.block,
            board.net,
            board.rtc,
            board.imsic,
            interrupt,
        ];
        for (index, region) in regions.iter().enumerate() {
            for other in &regions[..index] {
                assert!(!overlaps(region.base, region.size, other.base, other.size));
            }
        }
    }
}
