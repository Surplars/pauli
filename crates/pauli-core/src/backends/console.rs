//! VM UART input and bounded TX queue; physical output happens after unlocking.
//! Ownership/Lock Ordering: one VM device owner; drain copies bytes before logging.
//! Invariants: THRE reflects host-queue backpressure; binary bytes stay unformatted.
use pauli_control::{ConsoleChunk, Error};
use pauli_devices::uart16550::Uart16550;
const CAPACITY: usize = 1024;
const LOG_CAPACITY: usize = 4096;
const LSR: usize = 5;
const TRANSMITTER_EMPTY: u8 = 0x60;
pub struct Console {
    pub uart: Uart16550,
    tx: [u8; CAPACITY],
    head: usize,
    len: usize,
    pub dropped: u64,
    log: [u8; LOG_CAPACITY],
    total_written: u64,
    #[cfg(feature = "guest-control")]
    log_dirty: bool,
    // After exhausting absolute u64 cursors, TX and the circular buffer still progress.
    overflow_next: Option<usize>,
}
impl Console {
    pub const fn new(timeout: u64) -> Self {
        Self {
            uart: Uart16550::new(timeout),
            tx: [0; CAPACITY],
            head: 0,
            len: 0,
            dropped: 0,
            log: [0; LOG_CAPACITY],
            total_written: 0,
            #[cfg(feature = "guest-control")]
            log_dirty: false,
            overflow_next: None,
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
        // Every emitted byte enters history, including when host TX is full.
        #[cfg(feature = "guest-control")]
        {
            self.log_dirty = true;
        }
        let index = self.total_written as usize % LOG_CAPACITY;
        if let Some(total) = self.total_written.checked_add(1) {
            self.log[index] = byte;
            self.total_written = total;
        } else {
            let next = self.overflow_next.get_or_insert(index);
            self.log[*next] = byte;
            *next = (*next + 1) % LOG_CAPACITY;
        }
        if self.len == CAPACITY {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        self.tx[(self.head + self.len) % CAPACITY] = byte;
        self.len += 1;
    }
    pub(crate) fn read_log(&self, cursor: u64) -> Result<ConsoleChunk, Error> {
        if self.overflow_next.is_some() || cursor > self.total_written {
            return Err(Error::Invalid);
        }
        let oldest = self.total_written.saturating_sub(LOG_CAPACITY as u64);
        if cursor < oldest {
            return Ok(ConsoleChunk::Overrun { resume: oldest });
        }
        let mut data = [0; 6];
        let len = (self.total_written - cursor).min(data.len() as u64) as usize;
        let start = cursor as usize % LOG_CAPACITY;
        for (offset, dest) in data[..len].iter_mut().enumerate() {
            *dest = self.log[(start + offset) % LOG_CAPACITY];
        }
        Ok(ConsoleChunk::Bytes {
            data,
            len: len as u8,
        })
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
    #[cfg(feature = "guest-control")]
    pub(crate) fn take_log_dirty(&mut self) -> bool {
        core::mem::take(&mut self.log_dirty)
    }
}

#[cfg(test)]
mod tests {
    use super::{CAPACITY, Console, ConsoleChunk, Error, LOG_CAPACITY};

    #[test]
    fn history_records_every_emitted_byte_through_tx_backpressure_and_wrap() {
        let mut console = Console::new(5);
        console.access(3, Some(0x80)).unwrap(); // Divisor latch.
        console.access(0, Some(0xa5)).unwrap();
        console.access(3, Some(0)).unwrap();
        console.access(4, Some(0x10)).unwrap(); // Internal loopback.
        console.access(0, Some(0xff)).unwrap();
        console.access(4, Some(0)).unwrap();
        let eof = Ok(ConsoleChunk::Bytes {
            data: [0; 6],
            len: 0,
        });
        assert_eq!(console.read_log(0), eof);
        assert_eq!(console.read_log(1), Err(Error::Invalid));

        let byte = |i: usize| (i * 7 + (i / LOG_CAPACITY) * 13) as u8;
        let total = 2 * LOG_CAPACITY + 3;
        for i in 0..total {
            console.access(0, Some(byte(i).into())).unwrap();
        }
        let oldest = total - LOG_CAPACITY;
        for cursor in [0, oldest - 1] {
            assert_eq!(
                console.read_log(cursor as u64),
                Ok(ConsoleChunk::Overrun {
                    resume: oldest as u64
                })
            );
        }
        for cursor in (oldest..total).step_by(6).chain([2 * LOG_CAPACITY - 2]) {
            let len = (total - cursor).min(6);
            let mut data = [0; 6];
            for (offset, dest) in data[..len].iter_mut().enumerate() {
                *dest = byte(cursor + offset);
            }
            assert_eq!(
                console.read_log(cursor as u64),
                Ok(ConsoleChunk::Bytes {
                    data,
                    len: len as u8
                })
            );
        }
        assert_eq!(console.read_log(total as u64), eof);
        assert_eq!(console.read_log(total as u64 + 1), Err(Error::Invalid));
        assert!(console.is_full());
        assert_eq!(console.dropped, (total - CAPACITY) as u64);
        let mut tx = [0; CAPACITY];
        assert_eq!(console.drain(&mut tx), CAPACITY);
        assert!(tx.iter().enumerate().all(|(i, &b)| b == byte(i)));
        assert_eq!(console.read_log(total as u64), eof);
        console.reset(7);
        assert_eq!(console.read_log(0), eof);
        assert_eq!(console.read_log(oldest as u64), Err(Error::Invalid));
    }

    #[test]
    fn absolute_cursor_exhaustion_never_wraps_or_stalls_the_transmitter() {
        let mut console = Console::new(5);
        console.total_written = u64::MAX - 1;
        console.access(0, Some(1)).unwrap();
        assert_eq!(
            console.read_log(u64::MAX - 1),
            Ok(ConsoleChunk::Bytes {
                data: [1, 0, 0, 0, 0, 0],
                len: 1
            })
        );
        for byte in [2, 3] {
            console.access(0, Some(byte)).unwrap();
        }
        assert_eq!(console.total_written, u64::MAX);
        assert_eq!(console.read_log(u64::MAX), Err(Error::Invalid));
        assert_eq!((console.log[LOG_CAPACITY - 1], console.log[0]), (2, 3));
        let mut tx = [0; 3];
        assert_eq!(console.drain(&mut tx), 3);
        assert_eq!(tx, [1, 2, 3]);
    }

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
