//! Private native queue fixtures; RAM emulates registers/DMA, never a real disk.
//! Ownership/Lock Ordering: tests step the fake device only between driver calls.
//! Invariants: fake device writes only published slots; IDs/generations model adversarial returns.

use super::*;
use std::{
    alloc::{Layout, alloc_zeroed, handle_alloc_error},
    boxed::Box,
    vec::Vec,
};
struct FakeIrq;
// SAFETY: these single-threaded fixtures have no hardware interrupts to mask.
unsafe impl LocalIrq for FakeIrq {
    type Saved = ();
    fn save() {}
    unsafe fn restore(_: ()) {}
}
type Driver = VirtioBlk<FakeIrq>;

fn fixture() -> Driver {
    let words = Box::leak(Box::new([0u32; 128]));
    use pauli_virtio_common::regs;
    words[regs::MAGIC / 4] = regs::MAGIC_VALUE;
    words[regs::VERSION / 4] = regs::MODERN_VERSION;
    words[regs::DEVICE_ID / 4] = DEVICE_ID_BLOCK;
    words[regs::DEVICE_FEATURES / 4] = FEATURE_FLUSH | FeatureSet::MODERN;
    words[regs::QUEUE_NUM_MAX / 4] = QUEUE_SIZE as u32;
    words[regs::CONFIG / 4] = 4096;
    let resource = DeviceResource {
        base: words.as_mut_ptr() as usize,
        size: 512,
        irq: None,
    };
    let layout = Layout::from_size_align(DMA_BYTES, 4096).unwrap();
    // SAFETY: valid aligned layout, storage remains leaked/pinned for the fixture lifetime.
    let pointer = unsafe { alloc_zeroed(layout) };
    if pointer.is_null() {
        handle_alloc_error(layout);
    }
    // SAFETY: newly allocated exclusive initialized bytes, no actual physical DMA is performed.
    let dma = unsafe {
        DmaRegion::new(
            core::slice::from_raw_parts_mut(pointer, DMA_BYTES),
            pointer as u64,
            &pauli_hal::dma::COHERENT,
        )
        .unwrap()
    };
    // SAFETY: private initialized register memory and private pinned fake DMA allocation.
    unsafe { Driver::new(resource, dma).unwrap() }
}
fn request(operation: BlockOperation) -> BlockSubmission {
    BlockSubmission {
        operation,
        sector: 0,
        bytes: if operation == BlockOperation::Flush {
            0
        } else {
            512
        },
        deadline: 100,
        event: None,
    }
}
fn complete(driver: &Driver, token: BlockToken, seed: u8) {
    let request = driver.state.lock().slots[token.slot()].request;
    let offset = Driver::request_offset(token.slot());
    // SAFETY: sequential fake-device phase; it owns this published slot/ring element.
    unsafe {
        if request.operation == BlockOperation::Read {
            core::ptr::write_bytes(
                driver.dma.pointer(offset + HEADER_BYTES),
                seed,
                request.bytes,
            );
        }
        driver
            .dma
            .pointer(offset + STATUS_OFFSET)
            .write_volatile(STATUS_OK);
        let used = driver.used();
        let next = u16::from_le(core::ptr::addr_of!((*used).idx).read_volatile());
        core::ptr::addr_of_mut!((*used).ring[next as usize % QUEUE_SIZE]).write_volatile(
            pauli_virtio_common::VirtqUsedElem {
                id: ((token.slot() * CHAIN_SIZE) as u32).to_le(),
                len: (if request.operation == BlockOperation::Read {
                    (request.bytes + 1) as u32
                } else {
                    1
                })
                .to_le(),
            },
        );
        fence(Ordering::Release);
        core::ptr::addr_of_mut!((*used).idx).write_volatile(next.wrapping_add(1).to_le());
    }
}
fn fake_reset_ready(driver: &Driver) {
    // SAFETY: sequential RAM fixture emulates the native device's queue-ready reset side effect.
    unsafe {
        (driver.transport.base_addr() as *mut u32)
            .add(pauli_virtio_common::regs::QUEUE_READY / 4)
            .write_volatile(0);
    }
}

#[test]
fn four_inflight_out_of_order_and_flush_barrier() {
    let driver = fixture();
    let tokens: Vec<_> = (0..4)
        .map(|_| driver.submit(request(BlockOperation::Read), &[]).unwrap())
        .collect();
    assert_eq!(
        driver.submit(request(BlockOperation::Read), &[]),
        Err(DeviceError::Busy)
    );
    assert_eq!(
        driver.submit(request(BlockOperation::Flush), &[]),
        Err(DeviceError::Busy)
    );
    for index in [3, 1, 0, 2] {
        complete(&driver, tokens[index], index as u8);
    }
    assert_eq!(driver.service(0), Ok(4));
    for (index, token) in tokens.into_iter().enumerate() {
        assert_eq!(driver.try_poll(token), Ok(Some(512)));
        let mut out = [0; 512];
        assert_eq!(driver.collect(token, &mut out), Ok(512));
        assert_eq!(out, [index as u8; 512]);
        assert_eq!(driver.try_poll(token), Err(DeviceError::Stale));
    }
    let flush = driver.submit(request(BlockOperation::Flush), &[]).unwrap();
    assert_eq!(
        driver.submit(request(BlockOperation::Read), &[]),
        Err(DeviceError::Busy)
    );
    complete(&driver, flush, 0);
    assert_eq!(driver.service(0), Ok(1));
    assert_eq!(driver.collect(flush, &mut []), Ok(0));
}

#[test]
fn cancel_quarantines_and_generation_reuse_isolated() {
    let driver = fixture();
    let peer = fixture();
    let tokens: Vec<_> = (0..4)
        .map(|_| driver.submit(request(BlockOperation::Read), &[]).unwrap())
        .collect();
    for token in &tokens {
        driver.cancel(*token).unwrap();
    }
    assert_eq!(
        driver.submit(request(BlockOperation::Read), &[]),
        Err(DeviceError::Busy)
    );
    assert_eq!(peer.try_poll(tokens[0]), Err(DeviceError::Stale));
    complete(&driver, tokens[0], 99);
    assert_eq!(driver.service(0), Ok(1));
    let fresh = driver.submit(request(BlockOperation::Read), &[]).unwrap();
    assert_eq!(fresh.slot(), tokens[0].slot());
    assert_ne!(fresh.generation(), tokens[0].generation());
    assert_eq!(
        driver.collect(tokens[0], &mut [0; 512]),
        Err(DeviceError::Stale)
    );
    assert_eq!(driver.try_poll(fresh), Ok(None));
    let peer_token = peer.submit(request(BlockOperation::Read), &[]).unwrap();
    complete(&peer, peer_token, 7);
    assert_eq!(peer.service(0), Ok(1));
    assert_eq!(peer.collect(peer_token, &mut [0; 512]), Ok(512));
}

#[test]
fn reset_preserves_completed_data_and_retires_pending() {
    let driver = fixture();
    let completed = driver.submit(request(BlockOperation::Read), &[]).unwrap();
    let pending = driver.submit(request(BlockOperation::Read), &[]).unwrap();
    complete(&driver, completed, 0x6b);
    assert_eq!(driver.service(0), Ok(1));
    fake_reset_ready(&driver);
    assert_eq!(driver.service(100), Ok(0));
    assert_eq!(driver.try_poll(pending), Err(DeviceError::Timeout));
    assert_eq!(driver.collect(pending, &mut []), Err(DeviceError::Timeout));
    let mut out = [0; 512];
    assert_eq!(driver.collect(completed, &mut out), Ok(512));
    assert_eq!(out, [0x6b; 512]);
    let fresh = driver.submit(request(BlockOperation::Read), &[]).unwrap();
    complete(&driver, fresh, 0x42);
    assert_eq!(driver.service(101), Ok(1));
    assert_eq!(driver.collect(fresh, &mut out), Ok(512));
    assert_eq!(out, [0x42; 512]);
}
