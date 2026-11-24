//! Delayed private backend: pending is not completion or durability.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Read,
    Write,
    Flush,
}
#[derive(Debug, PartialEq, Eq)]
enum Event {
    Submit(Kind, u64, usize),
    Complete(Kind),
    Drain,
}
struct Active {
    kind: Kind,
    sector: u64,
    len: usize,
    data: Vec<u8>,
}
struct Delayed {
    disk: Disk,
    active: Option<Active>,
    ready: bool,
    blocked: bool,
    limit: usize,
    events: Vec<Event>,
    views: bool,
    view_calls: usize,
}
impl Delayed {
    fn call(&mut self, kind: Kind, sector: u64, data: &[u8], out: &mut [u8]) -> Poll<bool> {
        let len = if kind == Kind::Read {
            out.len()
        } else {
            data.len()
        };
        if let Some(active) = &self.active {
            assert_eq!(
                (active.kind, active.sector, active.len),
                (kind, sector, len)
            );
            if kind == Kind::Write {
                assert_eq!(active.data, data, "pending payload changed");
            }
            if !self.ready {
                return Poll::Pending;
            }
            return self.finish(out);
        }
        if self.blocked {
            return Poll::Pending;
        }
        self.events.push(Event::Submit(kind, sector, len));
        self.active = Some(Active {
            kind,
            sector,
            len,
            data: data.to_vec(),
        });
        Poll::Pending
    }
    fn finish(&mut self, out: &mut [u8]) -> Poll<bool> {
        let active = self.active.take().unwrap();
        self.events.push(Event::Complete(active.kind));
        match active.kind {
            Kind::Read => self.disk.read(0, active.sector, &mut out[..active.len]),
            Kind::Write => self.disk.write(0, active.sector, &active.data),
            Kind::Flush => self.disk.flush(0),
        }
    }
}
impl BlockBackend for Delayed {
    fn sectors(&self) -> u64 {
        self.disk.sectors
    }
    fn max_transfer_bytes(&self) -> usize {
        self.limit
    }
    fn read(&mut self, _slot: usize, sector: u64, out: &mut [u8]) -> Poll<bool> {
        self.call(Kind::Read, sector, &[], out)
    }
    fn read_with(
        &mut self,
        _slot: usize,
        sector: u64,
        bytes: usize,
        consume: &mut dyn FnMut(&[u8]) -> bool,
    ) -> Option<Poll<bool>> {
        if !self.views {
            return None;
        }
        // Fake CPU-owned transport storage, never the model's staging buffer.
        let mut data = vec![0; bytes];
        Some(self.call(Kind::Read, sector, &[], &mut data).map(|ok| {
            if !ok {
                return false;
            }
            self.view_calls += 1;
            consume(&data)
        }))
    }
    fn write(&mut self, _slot: usize, sector: u64, data: &[u8]) -> Poll<bool> {
        self.call(Kind::Write, sector, data, &mut [])
    }
    fn flush(&mut self, _slot: usize) -> Poll<bool> {
        self.call(Kind::Flush, 0, &[], &mut [])
    }
    fn drain(&mut self, slot: usize, scratch: &mut [u8]) -> Poll<()> {
        // These serial fixtures always occupy slot zero. Concurrent fake
        // handles and out-of-order completion live in concurrent_checks.
        if slot != 0 {
            return Poll::Ready(());
        }
        if self.active.is_none() {
            return Poll::Ready(());
        }
        if !self.ready {
            return Poll::Pending;
        }
        self.events.push(Event::Drain);
        self.finish(scratch).map(|_| ())
    }
}
struct Async<B = [u8; 4096 * MAX_INFLIGHT]> {
    device: VirtioBlock<B>,
    memory: Memory,
    backend: Delayed,
}
impl Async {
    fn new(writeback: bool) -> Self {
        let Fixture {
            device,
            memory,
            disk,
        } = writable(writeback);
        Self {
            device,
            memory,
            backend: Delayed {
                disk,
                active: None,
                ready: false,
                blocked: false,
                limit: 4096,
                events: Vec::new(),
                views: false,
                view_calls: 0,
            },
        }
    }
    fn large(batch: usize, buffer: usize) -> Async<Vec<u8>> {
        let small = Self::new(true);
        let mut device = VirtioBlock::with_buffer(256, true, vec![0; buffer * MAX_INFLIGHT]);
        let offered = features(&mut device);
        negotiate(&mut device, offered);
        queue(&mut device, DESC, AVAIL, USED);
        Async {
            device,
            memory: small.memory,
            backend: Delayed {
                limit: batch,
                ..small.backend
            },
        }
    }
}
impl<B: AsMut<[u8]>> Async<B> {
    fn service(&mut self) -> usize {
        self.device.service(&mut self.memory, &mut self.backend, 1)
    }
    fn post(&mut self, head: u16, kind: u32, sector: u64, len: usize) {
        let header = HEADER + u64::from(head) * 16;
        self.memory.header(header, kind, sector);
        self.memory.desc(head, header, 16, NEXT, head + 1);
        let mut next = head + 1;
        for offset in (0..len).step_by(4096) {
            let bytes = (len - offset).min(4096);
            self.memory.desc(
                next,
                DATA + u64::from(head) * 4096 + offset as u64,
                bytes as u32,
                NEXT | if kind == 0 { WRITE } else { 0 },
                next + 1,
            );
            next += 1;
        }
        self.memory
            .desc(next, STATUS + u64::from(head), 1, WRITE, 0);
        self.memory.put(STATUS + u64::from(head), &[0xff]);
        self.memory.post(head);
        write(&mut self.device, 0x50, 0);
    }
    fn pending(&self, head: u16, used: u16) {
        assert_eq!(self.memory.bytes(STATUS + u64::from(head), 1), [0xff]);
        assert_eq!(self.memory.word(USED + 2), used);
    }
    fn finish_one(&mut self) {
        self.backend.ready = true;
        for _ in 0..64 {
            if self.service() == 1 {
                return;
            }
        }
        panic!("bounded fake I/O did not complete");
    }
}

mod ordering;
mod transfers;
