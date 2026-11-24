//! Reservation, MMIO boundary and DMA lease validation against private host fixtures.
//! Ownership/Lock Ordering: each test exclusively owns all storage, no live devices.
//! Invariants: overlapping/overflowing ranges fail before any memory access.

use crate::{
    addr::{MmError, PAGE_SIZE},
    device::{DeviceError, DeviceResource},
    dma::{COHERENT, DmaRegion},
    mem::{PhysRange, PhysicalMemory},
    mmio::Mmio,
};
use std::{boxed::Box, vec};

#[test]
fn reserved_frames_and_retired_release() {
    let mut memory = PhysicalMemory::<4>::new(0x4000_0000, 16 * PAGE_SIZE).unwrap();
    let firmware = memory.reserve_range(0x4000_0000, PAGE_SIZE).unwrap();
    assert_eq!(
        memory.reserve_range(0x4000_0800, PAGE_SIZE),
        Err(MmError::AlreadyMapped)
    );
    let host = memory.allocate(2 * PAGE_SIZE, 2 * PAGE_SIZE).unwrap();
    let first = memory.allocate(2 * PAGE_SIZE, PAGE_SIZE).unwrap();
    let second = memory.allocate(2 * PAGE_SIZE, PAGE_SIZE).unwrap();
    for (a, b) in [(firmware, host), (host, first), (first, second)] {
        assert!(!a.overlaps(b));
    }
    assert_eq!(
        memory.allocate(PAGE_SIZE, PAGE_SIZE),
        Err(MmError::OutOfMemory)
    );
    // SAFETY: fixture ranges have never been installed into a page table or device.
    unsafe {
        memory.release_retired(first).unwrap();
    }
    assert_eq!(memory.reserve_range(first.base(), first.size()), Ok(first));
    assert_eq!(PhysRange::new(usize::MAX, 1), Err(MmError::InvalidRegion));
    assert_eq!(memory.reserve_range(0, 1), Err(MmError::InvalidRegion));
    assert_eq!(
        memory.allocate(1, PAGE_SIZE),
        Err(MmError::InvalidAlignment)
    );
}

#[test]
fn mmio_checks_alignment_and_width() {
    let mut words = [0u32; 4];
    let resource = DeviceResource {
        base: words.as_mut_ptr() as usize,
        size: 16,
        irq: None,
    };
    // SAFETY: exclusive local word storage is valid for the window's test lifetime.
    let mmio = unsafe { Mmio::new(resource, 16).unwrap() };
    mmio.write32(4, 0x1234).unwrap();
    assert_eq!(mmio.read32(4), Ok(0x1234));
    assert_eq!(mmio.read32(1), Err(DeviceError::InvalidResource));
    assert_eq!(mmio.read32(16), Err(DeviceError::InvalidResource));
    assert_eq!(mmio.read8(usize::MAX), Err(DeviceError::InvalidResource));
}

#[test]
fn capacity_counts_usable_holes_and_free_records_without_consuming_either() {
    let p = PAGE_SIZE;
    let mut memory = PhysicalMemory::<4>::new(0, 24 * p).unwrap();
    memory.reserve_range(4 * p, p).unwrap();
    memory.reserve_range(16 * p, p).unwrap();
    // Start addresses must be multiples of four pages. Only 8..14 fits six;
    // one-page requests instead exhaust the two remaining reservation records.
    assert_eq!(memory.allocation_capacity(6 * p, 4 * p), 1);
    assert_eq!(memory.allocation_capacity(p, 4 * p), 2);
    assert_eq!(memory.reservations().count(), 2);
    let extent = memory.allocate(6 * p, 4 * p).unwrap();
    assert_eq!(extent.base(), 8 * p);
    assert_eq!(memory.allocation_capacity(6 * p, 4 * p), 0);
    // SAFETY: this fixture only models addresses; nobody has mapped the extent.
    unsafe { memory.release_retired(extent).unwrap() };
    assert_eq!(memory.allocation_capacity(6 * p, 4 * p), 1);
}

#[test]
fn dma_ranges_do_not_accept_foreign_offsets() {
    let buffer = Box::leak(vec![0u8; 4096].into_boxed_slice());
    // SAFETY: private leaked storage stands in for coherent, contiguous driver memory.
    let dma = unsafe { DmaRegion::new(buffer, 0x2000, &COHERENT).unwrap() };
    assert_eq!(dma.device_address(4095), Ok(0x2fff));
    assert_eq!(dma.device_address(4096), Err(DeviceError::InvalidResource));
    assert_eq!(
        dma.for_cpu(usize::MAX, 2, crate::dma::Direction::FromDevice),
        Err(DeviceError::InvalidResource)
    );
}
