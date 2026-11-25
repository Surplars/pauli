//! UART byte/word MMIO layout checks on private RAM, not real UART registers.
//! Ownership/Lock Ordering: local fixture, no concurrency or live IRQs.
//! Invariants: reject unsupported stride/width before touching any register.

use super::*;
struct FakeIrq;
// SAFETY: local RAM fixture has no actual interrupt source.
unsafe impl LocalIrq for FakeIrq {
    type Saved = ();
    fn save() {}
    unsafe fn restore(_: ()) {}
}

#[test]
fn queued_hint_preserves_input_after_the_native_irq_has_cleared() {
    let mut words = [0u32; 8];
    let resource = DeviceResource {
        base: words.as_mut_ptr() as usize,
        size: size_of_val(&words),
        irq: None,
    };
    // SAFETY: private word-aligned registers remain live for this test.
    let uart = unsafe { Ns16550a::<FakeIrq>::new(resource, 2, 4, 0).unwrap() };
    uart.write(LSR, LSR_DR).unwrap();
    assert!(!uart.has_rx(), "readiness is not a hardware FIFO poll");
    // This fixed LSR supplies IRQ_BYTES identical bytes per bounded ISR call.
    for byte in b"ab" {
        uart.write(RBR, *byte).unwrap();
        assert!(uart.handle_irq());
    }
    uart.write(LSR, 0).unwrap();
    assert!(!uart.handle_irq());
    for byte in b"ab" {
        for _ in 0..IRQ_BYTES {
            assert!(uart.has_rx());
            assert_eq!(uart.read_byte(), Some(*byte));
        }
    }
    assert!(!uart.has_rx());
    assert_eq!(uart.read_byte(), None);
}

#[test]
fn word_stride_and_bounded_tx() {
    let mut words = [0u32; 8];
    let resource = DeviceResource {
        base: words.as_mut_ptr() as usize,
        size: 32,
        irq: None,
    };
    // SAFETY: private word-aligned RAM remains alive throughout the test.
    let uart = unsafe { Ns16550a::<FakeIrq>::new(resource, 2, 4, 3).unwrap() };
    assert_eq!(words[LCR], LCR_8N1 as u32);
    assert_eq!(words[IER], (IER_RX | IER_LINE) as u32);
    assert_eq!(uart.write_byte(b'x'), Err(DeviceError::Busy));
    words[LSR] = LSR_THRE as u32;
    assert_eq!(uart.write_byte(b'x'), Ok(()));
    assert_eq!(words[THR], b'x' as u32);
    // SAFETY: invalid layout is rejected before access to this same live fixture.
    assert!(unsafe { Ns16550a::<FakeIrq>::new(resource, 0, 4, 0) }.is_err());
}

#[test]
fn async_tx_bursts_fifo_arms_tx_irq_drains_on_irq_and_flushes() {
    let mut words = [0u32; 8];
    let resource = DeviceResource {
        base: words.as_mut_ptr() as usize,
        size: size_of_val(&words),
        irq: None,
    };
    // SAFETY: private word-aligned RAM remains alive throughout the test.
    let uart = unsafe { Ns16550a::<FakeIrq>::new(resource, 2, 4, 0).unwrap() };
    words[IIR] = IIR_FIFO as u32;
    uart.enable_async_tx();

    // Burst 16 bytes directly into THR while THRE is set; queue the remaining 4 bytes.
    words[LSR] = LSR_THRE as u32;
    let payload = b"0123456789abcdefghij";
    Console::write(&uart, payload).unwrap();
    assert_eq!(words[THR], payload[15] as u32);
    assert_eq!(words[IER], (IER_RX | IER_LINE | IER_TX) as u32);

    // While THRE is clear, handle_irq() leaves the ring intact and IER_TX armed.
    words[LSR] = 0;
    assert!(!uart.handle_irq());
    assert_eq!(words[IER], (IER_RX | IER_LINE | IER_TX) as u32);

    // Once THRE is set again, handle_irq() drains the 4 queued bytes and disarms IER_TX.
    words[LSR] = LSR_THRE as u32;
    assert!(uart.handle_irq());
    assert_eq!(words[THR], payload[19] as u32);
    assert_eq!(words[IER], (IER_RX | IER_LINE) as u32);

    // Queue another burst and verify flush_tx() drains it and disables async_tx.
    Console::write(&uart, b"ABCDEFGHIJKLMNOPQRSTUVWX").unwrap();
    assert_eq!(words[IER], (IER_RX | IER_LINE | IER_TX) as u32);
    words[LSR] = (LSR_THRE | LSR_TEMT) as u32;
    uart.flush_tx().unwrap();
    assert_eq!(words[THR], b'X' as u32);
    assert_eq!(words[IER], (IER_RX | IER_LINE) as u32);
    assert!(!uart.state.lock().async_tx);
}

#[test]
fn tx_poll_preserves_read_to_clear_errors_on_the_rx_head() {
    let mut words = [0u32; 8];
    // SAFETY: the register array stays at this address throughout the test.
    let uart = unsafe { fixture(&mut words) };
    uart.write(RBR, b'x').unwrap();
    uart.write(LSR, LSR_DR | LSR_BAD_BYTE).unwrap();
    assert_eq!(uart.write_byte(b'y'), Err(DeviceError::Busy));
    // The TX-side LSR read has cleared the real device's error flags.
    uart.write(LSR, LSR_DR).unwrap();
    assert!(uart.handle_irq());
    assert_eq!(uart.state.lock().len, IRQ_BYTES - 1);
}

#[test]
fn full_ring_makes_polling_progress_and_preserves_byte_order() {
    let mut words = [0u32; 8];
    // SAFETY: the register array stays at this address throughout the test.
    let uart = unsafe { fixture(&mut words) };
    words[IIR] = IIR_FIFO as u32;
    uart.enable_async_tx();
    let bytes = core::array::from_fn::<_, TX_CAPACITY, _>(|i| i as u8);
    Console::write(&uart, &bytes).unwrap();
    assert_eq!(uart.write_byte(b'x'), Err(DeviceError::Busy));
    words[LSR] = LSR_THRE as u32;
    uart.write_byte(b'x').unwrap();
    assert_eq!(words[THR], bytes[TX_FIFO_BURST - 1] as u32);
    Console::write(&uart, b"yz").unwrap();
    let state = uart.state.lock();
    assert_eq!(state.tx_head, TX_FIFO_BURST);
    assert_eq!(state.tx_len, TX_CAPACITY - TX_FIFO_BURST + 3);
    assert_eq!(&state.tx_bytes[..3], b"xyz");
    assert_eq!(words[IER], (IER_RX | IER_LINE | IER_TX) as u32);
}

#[test]
fn flush_timeout_retains_queue_and_waits_for_last_stop_bit() {
    let mut words = [0u32; 8];
    // SAFETY: the register array stays at this address throughout the test.
    let uart = unsafe { fixture(&mut words) };
    // This fixture leaves the FIFO detection bits at zero.
    uart.enable_async_tx();
    Console::write(&uart, b"ab").unwrap();
    assert_eq!(uart.flush_tx(), Err(DeviceError::Timeout));
    assert_eq!(uart.state.lock().tx_len, 2);
    assert_eq!(words[IER], (IER_RX | IER_LINE | IER_TX) as u32);
    words[LSR] = LSR_THRE as u32;
    uart.write_byte(b'c').unwrap();
    assert!(uart.handle_irq());
    assert_eq!(words[THR], b'a' as u32);
    assert_eq!(uart.state.lock().tx_len, 2);
    assert_eq!(uart.flush_tx(), Err(DeviceError::Timeout));
    assert_eq!(words[THR], b'c' as u32);
    assert!(uart.state.lock().async_tx);
    uart.write(LSR, LSR_THRE | LSR_TEMT).unwrap();
    uart.flush_tx().unwrap();
    assert!(!uart.state.lock().async_tx);
    let _guard = uart.state.lock();
    assert_eq!(uart.flush_tx(), Err(DeviceError::Busy));
}

/// The caller must keep the register array live at this address until the UART is dropped.
unsafe fn fixture(words: &mut [u32; 8]) -> Ns16550a<FakeIrq> {
    let resource = DeviceResource {
        base: words.as_mut_ptr() as usize,
        size: size_of_val(words),
        irq: None,
    };
    // SAFETY: each test keeps its private word-aligned registers alive until after the UART.
    unsafe { Ns16550a::new(resource, 2, 4, 0).unwrap() }
}
