#![no_std]
//! Native 16550 UART with bounded IRQ RX, polling TX, and optional IRQ-driven TX.
//! Ownership/Lock Ordering: short per-UART IRQ-safe lock; no log/callback while held.
//! Invariants: byte/word register layout is board supplied; RX queue never overwrites unread data.

use pauli_hal::{
    device::{DeviceError, DeviceResource, HostCharDevice, Result},
    irq::IrqHandler,
    log::Console,
    mmio::Mmio,
    sync::{IrqSpinLock, LocalIrq},
};
mod regs;
use regs::*;

const RX_CAPACITY: usize = 128;
const TX_CAPACITY: usize = 4096;
const TX_FIFO_BURST: usize = 16;
const IRQ_BYTES: usize = 32;
const TX_POLLS: usize = 100_000;
struct State {
    bytes: [u8; RX_CAPACITY],
    head: usize,
    len: usize,
    dropped: u64,
    rx_errors: u8,
    tx_bytes: [u8; TX_CAPACITY],
    tx_head: usize,
    tx_len: usize,
    tx_irq_enabled: bool,
    async_tx: bool,
    tx_burst: usize,
}
pub struct Ns16550a<I: LocalIrq> {
    mmio: Mmio,
    shift: u8,
    width: u8,
    state: IrqSpinLock<State, I>,
}

impl<I: LocalIrq> Ns16550a<I> {
    /// # Safety
    /// Resource must be a permanently mapped native UART with the described
    /// register stride/width. No other driver may reconfigure its registers.
    pub unsafe fn new(
        resource: DeviceResource,
        shift: u8,
        width: u8,
        divisor: u16,
    ) -> Result<Self> {
        if shift > 4
            || !matches!(width, 1 | 4)
            || (width == 4 && (shift < 2 || resource.base & 3 != 0))
        {
            return Err(DeviceError::InvalidResource);
        }
        let minimum = (MSR << shift) + width as usize;
        // SAFETY: caller's native mapping; register layout is checked above.
        let mmio = unsafe { Mmio::new(resource, minimum)? };
        let uart = Self {
            mmio,
            shift,
            width,
            state: IrqSpinLock::new(State {
                bytes: [0; RX_CAPACITY],
                head: 0,
                len: 0,
                dropped: 0,
                rx_errors: 0,
                tx_bytes: [0; TX_CAPACITY],
                tx_head: 0,
                tx_len: 0,
                tx_irq_enabled: false,
                async_tx: false,
                tx_burst: 1,
            }),
        };
        uart.write(IER, 0)?;
        if divisor != 0 {
            uart.write(LCR, LCR_DLAB)?;
            uart.write(RBR, divisor as u8)?;
            uart.write(IER, (divisor >> 8) as u8)?;
        }
        uart.write(LCR, LCR_8N1)?;
        uart.write(FCR, FCR_ENABLE | FCR_RESET_RX | FCR_RESET_TX)?;
        uart.write(MCR, MCR_DTR | MCR_RTS | MCR_OUT2)?;
        uart.write(IER, IER_RX | IER_LINE)?;
        Ok(uart)
    }
    /// The IRQ handler may have emptied the hardware FIFO before host WFI.
    /// Inspect its software queue without consuming a byte or touching MMIO.
    pub fn has_rx(&self) -> bool {
        self.state.lock().len != 0
    }
    /// Enable interrupt-driven TX ring buffering after the host IRQ route is live.
    pub fn enable_async_tx(&self) {
        let mut state = self.state.lock();
        if !state.async_tx {
            // IIR may acknowledge THRE. Sample only while the TX interrupt is off.
            state.tx_burst = if self.read(IIR).is_ok_and(|iir| iir & IIR_FIFO == IIR_FIFO) {
                TX_FIFO_BURST
            } else {
                1
            };
            state.async_tx = true;
        }
    }
    /// Drain software, FIFO and shifter, then return to polling. A panic can
    /// interrupt a lock holder, so report contention without waiting for it.
    pub fn flush_tx(&self) -> Result<()> {
        let mut state = self.state.try_lock().ok_or(DeviceError::Busy)?;
        while state.tx_len != 0 {
            let mut drained = false;
            for _ in 0..TX_POLLS {
                if self.drain_tx_fifo(&mut state)? {
                    drained = true;
                    break;
                }
                core::hint::spin_loop();
            }
            if !drained {
                return Err(DeviceError::Timeout);
            }
        }
        for _ in 0..TX_POLLS {
            if self.line_status(&mut state)? & LSR_TEMT != 0 {
                state.async_tx = false;
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(DeviceError::Timeout)
    }
    fn line_status(&self, state: &mut State) -> Result<u8> {
        let status = self.read(LSR)?;
        // LSR reads clear parity, framing and break on the current RX head,
        // including reads made only to check transmitter readiness.
        if status & LSR_DR != 0 {
            state.rx_errors |= status & LSR_BAD_BYTE;
        }
        Ok(status)
    }
    fn drain_tx_fifo(&self, state: &mut State) -> Result<bool> {
        if state.tx_len == 0 || self.line_status(state)? & LSR_THRE == 0 {
            return Ok(false);
        }
        let burst = state.tx_burst.min(state.tx_len);
        for _ in 0..burst {
            let byte = state.tx_bytes[state.tx_head];
            self.write(THR, byte)?;
            state.tx_head = (state.tx_head + 1) % TX_CAPACITY;
            state.tx_len -= 1;
        }
        if state.tx_len == 0 && state.tx_irq_enabled {
            self.write(IER, IER_RX | IER_LINE)?;
            state.tx_irq_enabled = false;
        }
        Ok(true)
    }
    fn try_write(&self, state: &mut State, bytes: &[u8]) -> Result<usize> {
        let mut sent = 0;
        if state.tx_len == 0 && self.line_status(state)? & LSR_THRE != 0 {
            let burst = if state.async_tx { state.tx_burst } else { 1 };
            sent = burst.min(bytes.len());
            for &byte in &bytes[..sent] {
                self.write(THR, byte)?;
            }
        }
        if !state.async_tx || sent == bytes.len() {
            return Ok(sent);
        }
        // A full ring can be drained even when the caller has masked IRQs.
        if state.tx_len == TX_CAPACITY {
            self.drain_tx_fifo(state)?;
        }
        let count = (bytes.len() - sent).min(TX_CAPACITY - state.tx_len);
        let tail = (state.tx_head + state.tx_len) % TX_CAPACITY;
        let first = count.min(TX_CAPACITY - tail);
        state.tx_bytes[tail..tail + first].copy_from_slice(&bytes[sent..sent + first]);
        state.tx_bytes[..count - first].copy_from_slice(&bytes[sent + first..sent + count]);
        state.tx_len += count;
        if !state.tx_irq_enabled {
            self.write(IER, IER_RX | IER_LINE | IER_TX)?;
            state.tx_irq_enabled = true;
        }
        Ok(sent + count)
    }
    fn read(&self, register: usize) -> Result<u8> {
        let offset = register << self.shift;
        if self.width == 1 {
            self.mmio.read8(offset)
        } else {
            self.mmio.read32(offset).map(|value| value as u8)
        }
    }
    fn write(&self, register: usize, value: u8) -> Result<()> {
        let offset = register << self.shift;
        if self.width == 1 {
            self.mmio.write8(offset, value)
        } else {
            self.mmio.write32(offset, value as u32)
        }
    }
    fn receive(&self, state: &mut State) -> Result<bool> {
        let mut received = false;
        for _ in 0..IRQ_BYTES {
            let status = self.line_status(state)?;
            if status & LSR_DR == 0 {
                break;
            }
            let byte = self.read(RBR)?;
            if core::mem::take(&mut state.rx_errors) != 0 {
                continue;
            }
            if state.len == RX_CAPACITY {
                state.dropped = state.dropped.wrapping_add(1);
            } else {
                state.bytes[(state.head + state.len) % RX_CAPACITY] = byte;
                state.len += 1;
            }
            received = true;
        }
        Ok(received)
    }
}
impl<I: LocalIrq> HostCharDevice for Ns16550a<I> {
    fn read_byte(&self) -> Option<u8> {
        let mut state = self.state.lock();
        if state.len == 0 {
            return None;
        }
        let byte = state.bytes[state.head];
        state.head = (state.head + 1) % RX_CAPACITY;
        state.len -= 1;
        Some(byte)
    }
    fn write_byte(&self, byte: u8) -> Result<()> {
        if self.try_write(&mut self.state.lock(), &[byte])? == 0 {
            Err(DeviceError::Busy)
        } else {
            Ok(())
        }
    }
}
impl<I: LocalIrq> IrqHandler for Ns16550a<I> {
    fn handle_irq(&self) -> bool {
        let mut state = self.state.lock();
        let rx = self.receive(&mut state).unwrap_or(false);
        let tx = self.drain_tx_fifo(&mut state).unwrap_or(false);
        rx || tx
    }
}
impl<I: LocalIrq> Console for Ns16550a<I> {
    fn write(&self, bytes: &[u8]) -> core::fmt::Result {
        let mut remaining = bytes;
        let mut stalled = 0;
        while !remaining.is_empty() {
            let count = self
                .try_write(&mut self.state.lock(), remaining)
                .map_err(|_| core::fmt::Error)?;
            if count == 0 {
                stalled += 1;
                if stalled == TX_POLLS {
                    return Err(core::fmt::Error);
                }
                core::hint::spin_loop();
            } else {
                remaining = &remaining[count..];
                stalled = 0;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
