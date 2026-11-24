//! Default virtual board ABI and mutually exclusive interrupt topologies.
//! Ownership/Lock Ordering: immutable local values; no locks or host resources.
//! Invariants: peripheral regions are disjoint; APLIC replaces, not augments, PLIC.

use super::*;
use crate::{GuestBoardLayout, MAX_GUEST_VCPUS, MmioRegion, overlaps};

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
