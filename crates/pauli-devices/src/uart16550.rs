//! Bounded 16550A console model, serialized by its owning VM dispatcher.
//! TX completes immediately; RX has a 16-byte FIFO, sticky overrun, FIFO
//! triggers and a coarse configurable character timeout. Time units and
//! physical transport belong to the caller, allowing RISC-V/arm64 reuse.
//! Ownership/Lock Ordering: exclusive device owner, no locks or transport callbacks.
//! Invariants: FIFO admission is bounded and register reads consume/ack only once.

const DATA: usize = 0;
const INTERRUPT_ENABLE: usize = 1;
const INTERRUPT_ID_FIFO: usize = 2;
const LINE_CONTROL: usize = 3;
const MODEM_CONTROL: usize = 4;
const LINE_STATUS: usize = 5;
const MODEM_STATUS: usize = 6;
const SCRATCH: usize = 7;
const IER_RX: u8 = 1;
const IER_TX: u8 = 2;
const IER_LINE: u8 = 4;
const IER_MODEM: u8 = 8;
const IER_MASK: u8 = IER_RX | IER_TX | IER_LINE | IER_MODEM;
const IIR_MODEM: u8 = 0;
const IIR_NONE: u8 = 1;
const IIR_TX: u8 = 2;
const IIR_RX: u8 = 4;
const IIR_LINE: u8 = 6;
const IIR_TIMEOUT: u8 = 12;
const IIR_FIFO: u8 = 0xc0;
const FCR_ENABLE: u8 = 1;
const FCR_RESET_RX: u8 = 2;
const FCR_RESET_TX: u8 = 4;
const FCR_TRIGGER_SHIFT: u8 = 6;
const FCR_MASK: u8 = FCR_ENABLE | (3 << FCR_TRIGGER_SHIFT);
const LCR_DLAB: u8 = 0x80;
const MCR_DTR: u8 = 1;
const MCR_RTS: u8 = 2;
const MCR_OUT1: u8 = 4;
const MCR_OUT2: u8 = 8;
const MCR_LOOP: u8 = 16;
const MCR_MASK: u8 = MCR_DTR | MCR_RTS | MCR_OUT1 | MCR_OUT2 | MCR_LOOP;
const MSR_DCTS_DDSR: u8 = 3;
const MSR_CTS: u8 = 0x10;
const MSR_DSR: u8 = 0x20;
const MSR_RI: u8 = 0x40;
const MSR_DCD: u8 = 0x80;
const LSR_EMPTY: u8 = 0x60;
const LSR_OVERRUN: u8 = 2;

pub struct Uart16550 {
    ier: u8,
    fcr: u8,
    lcr: u8,
    mcr: u8,
    scratch: u8,
    divisor: [u8; 2],
    rx: [u8; 16],
    head: usize,
    len: usize,
    overrun: bool,
    tx_pending: bool,
    modem_delta: u8,
    now: u64,
    last_rx: u64,
    timeout: u64,
    pub received: u64,
    pub overruns: u64,
}
impl Uart16550 {
    pub const fn new(rx_timeout_ticks: u64) -> Self {
        Self {
            ier: 0,
            fcr: 0,
            lcr: 0,
            mcr: 0,
            scratch: 0,
            divisor: [0; 2],
            rx: [0; 16],
            head: 0,
            len: 0,
            overrun: false,
            tx_pending: false,
            modem_delta: 0,
            now: 0,
            last_rx: 0,
            timeout: rx_timeout_ticks,
            received: 0,
            overruns: 0,
        }
    }
    pub fn advance(&mut self, now: u64) {
        self.now = now;
    }
    pub fn rx_has_room(&self) -> bool {
        self.len < if self.fcr & FCR_ENABLE != 0 { 16 } else { 1 }
    }
    pub fn receive(&mut self, byte: u8) -> bool {
        if !self.rx_has_room() {
            self.overrun = true;
            self.overruns += 1;
            return false;
        }
        self.rx[(self.head + self.len) % 16] = byte;
        self.len += 1;
        self.last_rx = self.now;
        self.received += 1;
        true
    }
    fn trigger(&self) -> usize {
        if self.fcr & FCR_ENABLE == 0 {
            1
        } else {
            [1, 4, 8, 14][(self.fcr >> FCR_TRIGGER_SHIFT) as usize]
        }
    }
    fn interrupt_id(&self) -> u8 {
        if self.ier & IER_LINE != 0 && self.overrun {
            return IIR_LINE;
        }
        if self.ier & IER_RX != 0 && self.len >= self.trigger() {
            return IIR_RX;
        }
        if self.ier & IER_RX != 0
            && self.len != 0
            && self.now.saturating_sub(self.last_rx) >= self.timeout
        {
            return IIR_TIMEOUT;
        }
        if self.ier & IER_TX != 0 && self.tx_pending {
            return IIR_TX;
        }
        if self.ier & IER_MODEM != 0 && self.modem_delta != 0 {
            return IIR_MODEM;
        }
        IIR_NONE
    }
    pub fn irq_pending(&self) -> bool {
        self.interrupt_id() & IIR_NONE == 0
    }
    /// Classify RX without IIR's THRE acknowledgement side effect.
    pub fn rx_irq_pending(&self) -> bool {
        matches!(self.interrupt_id(), IIR_RX | IIR_TIMEOUT)
    }
    /// Only a partially filled FIFO can turn elapsed time into a new RX IRQ.
    pub fn rx_timeout_armed(&self) -> bool {
        self.len != 0
            && self.ier & IER_RX != 0
            && self.len < self.trigger()
            && self.now.saturating_sub(self.last_rx) < self.timeout
    }
    fn modem(&self) -> u8 {
        if self.mcr & MCR_LOOP == 0 {
            MSR_CTS | MSR_DSR | MSR_DCD
        } else {
            ((self.mcr & MCR_RTS) << 3)
                | ((self.mcr & MCR_DTR) << 5)
                | ((self.mcr & MCR_OUT1) << 4)
                | ((self.mcr & MCR_OUT2) << 4)
        }
    }
    /// Eight byte-wide register offsets. Unsupported offsets return None;
    /// reading IIR acknowledges only THRE, reading RBR consumes exactly once.
    pub fn read(&mut self, offset: usize) -> Option<u8> {
        if self.lcr & LCR_DLAB != 0 && offset < INTERRUPT_ID_FIFO {
            return Some(self.divisor[offset]);
        }
        Some(match offset {
            DATA => {
                if self.len == 0 {
                    0
                } else {
                    let byte = self.rx[self.head];
                    self.head = (self.head + 1) % 16;
                    self.len -= 1;
                    self.last_rx = self.now;
                    byte
                }
            }
            INTERRUPT_ENABLE => self.ier,
            INTERRUPT_ID_FIFO => {
                let id = self.interrupt_id();
                if id == IIR_TX {
                    self.tx_pending = false;
                }
                id | if self.fcr & FCR_ENABLE != 0 {
                    IIR_FIFO
                } else {
                    0
                }
            }
            LINE_CONTROL => self.lcr,
            MODEM_CONTROL => self.mcr,
            LINE_STATUS => {
                let value = LSR_EMPTY
                    | u8::from(self.len != 0)
                    | if self.overrun { LSR_OVERRUN } else { 0 };
                self.overrun = false;
                value
            }
            MODEM_STATUS => {
                let value = self.modem() | self.modem_delta;
                self.modem_delta = 0;
                value
            }
            SCRATCH => self.scratch,
            _ => return None,
        })
    }
    /// Returns an output byte for the caller's transport, if any. Loopback
    /// feeds RX instead; divisor writes never leak into the output stream.
    pub fn write(&mut self, offset: usize, value: u8) -> Option<Option<u8>> {
        if self.lcr & LCR_DLAB != 0 && offset < INTERRUPT_ID_FIFO {
            self.divisor[offset] = value;
            return Some(None);
        }
        match offset {
            DATA => {
                self.tx_pending = true;
                if self.mcr & MCR_LOOP != 0 {
                    self.receive(value);
                } else {
                    return Some(Some(value));
                }
            }
            INTERRUPT_ENABLE => {
                if self.ier & IER_TX == 0 && value & IER_TX != 0 {
                    self.tx_pending = true;
                }
                self.ier = value & IER_MASK;
            }
            INTERRUPT_ID_FIFO => {
                if (self.fcr ^ value) & FCR_ENABLE != 0 || value & FCR_RESET_RX != 0 {
                    self.head = 0;
                    self.len = 0;
                    self.overrun = false;
                }
                if value & FCR_RESET_TX != 0 {
                    self.tx_pending = true;
                }
                self.fcr = value & FCR_MASK;
            }
            LINE_CONTROL => self.lcr = value,
            MODEM_CONTROL => {
                let old = self.modem();
                self.mcr = value & MCR_MASK;
                let new = self.modem();
                let changed = old ^ new;
                self.modem_delta |= ((changed >> 4) & MSR_DCTS_DDSR)
                    | ((old & !new & MSR_RI) >> 4)
                    | ((changed & MSR_DCD) >> 4);
            }
            LINE_STATUS | MODEM_STATUS => {} // read-only status registers
            SCRATCH => self.scratch = value,
            _ => return None,
        }
        Some(None)
    }
}
