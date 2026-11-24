#![no_std]
//! Native 16550 UART with bounded IRQ RX and polling TX, no RTOS terminal layer.
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
const IRQ_BYTES: usize = 32;
const TX_POLLS: usize = 100_000;
struct State {
    bytes: [u8; RX_CAPACITY],
    head: usize,
    len: usize,
    dropped: u64,
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
            let status = self.read(LSR)?;
            if status & LSR_DR == 0 {
                break;
            }
            let byte = self.read(RBR)?;
            if status & LSR_BAD_BYTE != 0 {
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
        let _owner = self.state.lock();
        if self.read(LSR)? & LSR_THRE == 0 {
            return Err(DeviceError::Busy);
        }
        self.write(THR, byte)
    }
}
impl<I: LocalIrq> IrqHandler for Ns16550a<I> {
    fn handle_irq(&self) -> bool {
        let mut state = self.state.lock();
        self.receive(&mut state).unwrap_or(false)
    }
}
impl<I: LocalIrq> Console for Ns16550a<I> {
    fn write(&self, bytes: &[u8]) -> core::fmt::Result {
        for &byte in bytes {
            let mut sent = false;
            for _ in 0..TX_POLLS {
                match self.write_byte(byte) {
                    Ok(()) => {
                        sent = true;
                        break;
                    }
                    Err(DeviceError::Busy) => core::hint::spin_loop(),
                    Err(_) => return Err(core::fmt::Error),
                }
            }
            if !sent {
                return Err(core::fmt::Error);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
