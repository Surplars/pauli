//! Multi-slot ordering and notification race assertions.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

//! Four independently delayed native handles. Completion order, errors and
//! reset drains are controlled explicitly; no host disk or guest execution.
use super::*;

struct Active {
    kind: u32,
    sector: u64,
    len: usize,
    data: Vec<u8>,
}
pub(super) struct Backend {
    pub(super) disk: Disk,
    slots: [Option<Active>; MAX_INFLIGHT],
    pub(super) ready: u8,
    busy: u8,
    fail: u8,
    pub(super) starts: Vec<(usize, u32, u64)>,
    pub(super) drains: usize,
}
impl Backend {
    fn call(
        &mut self,
        slot: usize,
        kind: u32,
        sector: u64,
        data: &[u8],
        out: &mut [u8],
    ) -> Poll<bool> {
        let len = if kind == 0 { out.len() } else { data.len() };
        if let Some(active) = &self.slots[slot] {
            assert_eq!(
                (active.kind, active.sector, active.len),
                (kind, sector, len)
            );
            if kind == 1 {
                assert_eq!(active.data, data, "slot snapshot changed");
            }
            return self.finish(slot, out);
        }
        if self.busy & (1 << slot) != 0 {
            return Poll::Pending;
        }
        if kind == 4 {
            assert!(
                self.slots.iter().all(Option::is_none),
                "flush passed earlier data"
            );
        }
        self.starts.push((slot, kind, sector));
        self.slots[slot] = Some(Active {
            kind,
            sector,
            len,
            data: data.to_vec(),
        });
        Poll::Pending
    }
    fn finish(&mut self, slot: usize, out: &mut [u8]) -> Poll<bool> {
        if self.ready & (1 << slot) == 0 {
            return Poll::Pending;
        }
        self.ready &= !(1 << slot);
        let active = self.slots[slot].take().unwrap();
        if self.fail & (1 << slot) != 0 {
            return Poll::Ready(false);
        }
        match active.kind {
            0 => self.disk.read(slot, active.sector, &mut out[..active.len]),
            1 => self.disk.write(slot, active.sector, &active.data),
            4 => self.disk.flush(slot),
            _ => unreachable!(),
        }
    }
}
impl BlockBackend for Backend {
    fn sectors(&self) -> u64 {
        self.disk.sectors
    }
    fn read(&mut self, slot: usize, sector: u64, out: &mut [u8]) -> Poll<bool> {
        self.call(slot, 0, sector, &[], out)
    }
    fn write(&mut self, slot: usize, sector: u64, data: &[u8]) -> Poll<bool> {
        self.call(slot, 1, sector, data, &mut [])
    }
    fn flush(&mut self, slot: usize) -> Poll<bool> {
        self.call(slot, 4, 0, &[], &mut [])
    }
    fn drain(&mut self, slot: usize, scratch: &mut [u8]) -> Poll<()> {
        if self.slots[slot].is_none() {
            return Poll::Ready(());
        }
        self.finish(slot, scratch).map(|_| {
            self.drains += 1;
        })
    }
}
pub(super) struct Fixture4 {
    pub(super) device: VirtioBlock,
    pub(super) memory: Memory,
    pub(super) backend: Backend,
}
impl Fixture4 {
    pub(super) fn new(writeback: bool) -> Self {
        let Fixture {
            device,
            memory,
            disk,
        } = writable(writeback);
        Self {
            device,
            memory,
            backend: Backend {
                disk,
                slots: [const { None }; MAX_INFLIGHT],
                ready: 0,
                busy: 0,
                fail: 0,
                starts: Vec::new(),
                drains: 0,
            },
        }
    }
    pub(super) fn post(&mut self, n: u16, kind: u32) {
        let head = n * 3;
        let header = HEADER + u64::from(n) * 16;
        self.memory.header(header, kind, u64::from(n) * 8);
        self.memory.desc(head, header, 16, NEXT, head + 1);
        if kind != 4 {
            self.memory.desc(
                head + 1,
                DATA + u64::from(n) * 4096,
                512,
                NEXT | if kind == 0 { WRITE } else { 0 },
                head + 2,
            );
        }
        self.memory.desc(
            head + if kind == 4 { 1 } else { 2 },
            STATUS + u64::from(n),
            1,
            WRITE,
            0,
        );
        self.memory.put(STATUS + u64::from(n), &[0xff]);
        self.memory.post(head);
        write(&mut self.device, 0x50, 0);
    }
    pub(super) fn service(&mut self) -> usize {
        self.device.service(&mut self.memory, &mut self.backend, 4)
    }
    fn status(&self, n: usize) -> u8 {
        self.memory.bytes(STATUS + n as u64, 1)[0]
    }
}

#[test]
fn reordered_batch() {
    let mut f = Fixture4::new(true);
    for n in 0..5 {
        f.post(n, 0);
    }
    assert_eq!(f.service(), 0);
    assert_eq!(f.backend.starts.len(), 4);
    assert_eq!(f.device.peak_inflight, 4);
    f.backend.ready = 0b1100;
    assert_eq!(f.service(), 2);
    assert_eq!(
        (f.status(0), f.status(1), f.status(2), f.status(3)),
        (255, 255, 0, 0)
    );
    assert_eq!(f.memory.used_publications, 1);
    assert_eq!((f.device.largest_batch, f.device.irq_edges), (2, 1));
    assert_eq!(f.memory.dword(USED + 4), 6);
    assert_eq!(f.memory.dword(USED + 12), 9);
    write(&mut f.device, 0x64, 1);
    assert_eq!(f.service(), 0); // Free slot reused while the oldest read waits.
    assert_eq!(f.backend.starts.len(), 5);
    f.backend.ready = 0b0111;
    assert_eq!(f.service(), 3);
    assert_eq!(
        (
            f.device.completed,
            f.device.completion_batches,
            f.device.irq_edges
        ),
        (5, 2, 2)
    );
    assert_eq!(f.memory.used_publications, 2);
    for n in 0..5 {
        assert_eq!(f.status(n), 0);
        assert_eq!(
            f.memory.bytes(DATA + n as u64 * 4096, 512),
            (0..512)
                .map(|i| pattern(n as u64 * 4096 + i))
                .collect::<Vec<_>>()
        );
    }
    // Suppression is honoured for a whole batch, not just its first element.
    let mut f = Fixture4::new(true);
    f.memory.put(AVAIL, &1u16.to_le_bytes());
    for n in 0..4 {
        f.post(n, 0);
    }
    f.service();
    f.backend.ready = 15;
    assert_eq!(f.service(), 4);
    assert!(!f.device.irq_pending());
    assert_eq!(f.memory.used_publications, 1);

    for (before, after) in [(1u16, 0u16), (0, 1)] {
        let mut f = Fixture4::new(true);
        f.memory.put(AVAIL, &before.to_le_bytes());
        f.memory.flags_on_publication = Some(after);
        for n in 0..4 {
            f.post(n, 0);
        }
        f.service();
        f.backend.ready = 15;
        assert_eq!(f.service(), 4);
        assert_eq!(
            f.device.irq_pending(),
            after == 0,
            "notification decision must follow used.idx publication"
        );
    }
}

#[test]
fn barriers_and_snapshots() {
    let mut f = Fixture4::new(true);
    for n in 0..2 {
        f.memory.put(DATA + n * 4096, &[0x31 + n as u8; 512]);
        f.post(n as u16, 1);
    }
    f.post(2, 4);
    f.post(3, 1);
    f.service();
    assert_eq!(f.backend.starts.len(), 2);
    f.memory.put(DATA, &[0xee; 512]); // Staging survives sibling service calls.
    f.backend.ready = 2;
    assert_eq!(f.service(), 1);
    assert_eq!(f.backend.starts.len(), 2);
    f.backend.ready = 1;
    assert_eq!(f.service(), 1);
    f.service();
    assert_eq!(f.backend.starts.last().unwrap().1, 4);
    assert_eq!(f.backend.starts.len(), 3);
    assert_eq!(f.backend.disk.writes[1].1, vec![0x31; 512]);
    let slot = f.backend.starts.last().unwrap().0;
    f.backend.ready = 1 << slot;
    assert_eq!(f.service(), 1);
    assert_eq!(f.backend.disk.flush_calls, 1);
    assert_eq!(f.backend.starts.last().unwrap().1, 1);
    // Without negotiated flush, each write runs alone through durable flush.
    let mut f = Fixture4::new(false);
    f.post(0, 1);
    f.post(1, 1);
    f.service();
    assert_eq!(f.backend.starts.len(), 1);
    f.backend.ready = 1;
    assert_eq!(f.service(), 0);
    assert_eq!(f.backend.starts[1].1, 4);
    f.backend.ready = 1;
    assert_eq!(f.service(), 1);
    assert_eq!(f.backend.disk.flush_calls, 1);
}

#[test]
fn reset_fault_and_peer() {
    for broken in [false, true] {
        let mut f = Fixture4::new(true);
        for n in 0..4 {
            f.post(n, 0);
        }
        f.service();
        if broken {
            write(&mut f.device, 0x80, DESC as u32);
        } else {
            write(&mut f.device, 0x70, 0);
        }
        f.memory = Memory::new();
        f.memory.put(DATA, &[0xcc; 16384]);
        // Reset/restart installs a new queue before old operations finish.
        if !broken {
            let offered = features(&mut f.device);
            negotiate(&mut f.device, offered);
            queue(&mut f.device, DESC, AVAIL, USED);
            f.post(0, 0);
        }
        f.backend.ready = 0b1010;
        assert_eq!(f.service(), 0);
        assert_eq!(f.backend.starts.len(), 4);
        assert_eq!(f.backend.drains, 2);
        assert_eq!(f.memory.bytes(DATA, 16384), &[0xcc; 16384]);
        f.backend.ready = 0b0101;
        assert_eq!(f.service(), 0);
        assert_eq!(f.backend.drains, 4);
        assert_eq!(f.memory.word(USED + 2), 0);
        assert_eq!(f.memory.bytes(DATA, 16384), &[0xcc; 16384]);
        if !broken {
            assert_eq!(f.backend.starts.len(), 5);
            f.backend.ready = 1;
            assert_eq!(f.service(), 1);
            assert_eq!(f.device.completed, 1);
        }
    }
    let mut slow = Fixture4::new(true);
    let mut peer = Fixture4::new(true);
    for n in 0..4 {
        slow.post(n, 0);
        peer.post(n, 0);
    }
    slow.service();
    peer.service();
    peer.backend.ready = 15;
    peer.backend.fail = 2;
    assert_eq!(peer.service(), 4);
    assert_eq!(peer.status(1), 1);
    assert_eq!(peer.status(2), 0);
    assert_eq!(slow.device.completed, 0);
    assert!(slow.backend.slots.iter().all(Option::is_some));
    // Revoked guest write permission faults this queue without touching peer.
    slow.memory.read_only = Some((DATA, 16384));
    slow.backend.ready = 15;
    assert_eq!(slow.service(), 0);
    assert_ne!(read(&slow.device, 0x70) & 64, 0);
    assert_eq!(peer.device.completed, 4);
    slow.service();
    assert!(slow.backend.slots.iter().all(Option::is_none));
}

#[test]
fn admission_and_fairness() {
    let mut f = Fixture4::new(true);
    f.backend.busy = 1;
    for n in 0..4 {
        f.post(n, 1);
    }
    f.service();
    assert_eq!(f.backend.starts.len(), 3);
    f.memory.put(DATA, &[0xff; 512]);
    f.backend.busy = 0;
    f.service();
    f.backend.ready = 15;
    for _ in 0..4 {
        f.device.service(&mut f.memory, &mut f.backend, 1);
    }
    assert_eq!(f.device.completed, 4);
    assert!(
        f.backend
            .disk
            .writes
            .iter()
            .all(|(_, data)| *data == vec![0; 512])
    );
    let mut f = Fixture4::new(true);
    for n in 0..4 {
        f.post(n, 0);
    }
    f.service();
    f.backend.ready = 8;
    for _ in 0..4 {
        f.device.service(&mut f.memory, &mut f.backend, 1);
    }
    assert_eq!(
        f.status(3),
        0,
        "small budgets must not starve ready siblings"
    );
    // Duplicate active descriptor ownership is rejected at guest admission.
    f.memory.post(0);
    write(&mut f.device, 0x50, 0);
    f.service();
    assert_ne!(read(&f.device, 0x70) & 64, 0);
}
