//! VM UART input and bounded TX queue; physical output happens after unlocking.
//! Ownership/Lock Ordering: one VM device owner; drain copies bytes before logging.
//! Invariants: THRE reflects host-queue backpressure; binary bytes stay unformatted.
use pauli_devices::uart16550::Uart16550;
const CAPACITY: usize = 1024;
const LSR: usize = 5;
const TRANSMITTER_EMPTY: u8 = 0x60;
pub struct Console {
    pub uart: Uart16550,
    tx: [u8; CAPACITY],
    head: usize,
    len: usize,
    pub dropped: u64,
}
impl Console {
    pub const fn new(timeout: u64) -> Self {
        Self {
            uart: Uart16550::new(timeout),
            tx: [0; CAPACITY],
            head: 0,
            len: 0,
            dropped: 0,
        }
    }
    pub fn access(&mut self, offset: usize, write: Option<usize>) -> Option<usize> {
        match write {
            Some(value) => {
                if let Some(byte) = self.uart.write(offset, value as u8)? {
                    self.push(byte);
                }
                Some(0)
            }
            None => {
                let mut value = self.uart.read(offset)?;
                if offset == LSR && self.len == CAPACITY {
                    value &= !TRANSMITTER_EMPTY;
                }
                Some(usize::from(value))
            }
        }
    }
    fn push(&mut self, byte: u8) {
        if self.len == CAPACITY {
            self.dropped += 1;
            return;
        }
        self.tx[(self.head + self.len) % CAPACITY] = byte;
        self.len += 1;
    }
    pub fn drain(&mut self, output: &mut [u8]) -> usize {
        let count = output.len().min(self.len);
        for byte in &mut output[..count] {
            *byte = self.tx[self.head];
            self.head = (self.head + 1) % CAPACITY;
        }
        self.len -= count;
        count
    }
    pub(crate) fn has_output(&self) -> bool {
        self.len != 0
    }
    pub(crate) fn is_full(&self) -> bool {
        self.len == CAPACITY
    }
    pub(crate) fn needs_fast_idle_service(&self) -> bool {
        self.has_output() || self.uart.rx_timeout_armed()
    }
    pub fn reset(&mut self, timeout: u64) {
        *self = Self::new(timeout);
    }
}

#[cfg(test)]
mod tests {
    use super::Console;

    #[test]
    fn idle_service_tracks_tx_backlog_and_rx_deadline() {
        let mut console = Console::new(5);
        assert!(!console.needs_fast_idle_service());
        for byte in b"ab" {
            console.access(0, Some(usize::from(*byte))).unwrap();
        }
        assert_eq!(console.drain(&mut [0]), 1);
        assert!(console.needs_fast_idle_service());
        assert_eq!(console.drain(&mut [0]), 1);
        assert!(!console.needs_fast_idle_service());

        console.access(2, Some(0x41)).unwrap(); // FIFO trigger: four bytes.
        assert!(console.uart.receive(b'x'));
        assert!(!console.needs_fast_idle_service(), "RX is masked");
        console.access(1, Some(1)).unwrap();
        console.uart.advance(4);
        assert!(console.needs_fast_idle_service());
        console.uart.advance(5);
        assert!(console.uart.rx_irq_pending());
        assert!(!console.needs_fast_idle_service(), "timeout already raised");
        for byte in b"yzw" {
            assert!(console.uart.receive(*byte));
        }
        assert!(
            !console.needs_fast_idle_service(),
            "trigger already reached"
        );
        console.access(0, None).unwrap();
        assert!(console.needs_fast_idle_service(), "read restarts timeout");
        console.reset(5);
        assert!(!console.needs_fast_idle_service());
    }
}
