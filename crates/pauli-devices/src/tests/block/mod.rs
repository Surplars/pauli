//! VM-private fake memory and storage fixtures, no native DMA.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

use super::*;
use crate::virtio_blk::{BlockBackend, GuestMemory, MAX_INFLIGHT, VirtioBlock};
use core::task::Poll;

// Arbitrary mock RAM; no QEMU/host physical topology is required by the model.
const BASE: u64 = 0x4000_0000;
const DESC: u64 = BASE + 0x1000;
const AVAIL: u64 = BASE + 0x2000;
const USED: u64 = BASE + 0x3000;
const HEADER: u64 = BASE + 0x4000;
const STATUS: u64 = BASE + 0x5000;
const DATA: u64 = BASE + 0x6000;
const QUEUE_SIZE: u16 = 64;
const NEXT: u16 = 1;
const WRITE: u16 = 2;
const INDIRECT: u16 = 4;

struct Memory {
    bytes: Vec<u8>,
    read_only: Option<(u64, usize)>,
    used_publications: usize,
    flags_on_publication: Option<u16>,
    payload_read: core::cell::Cell<usize>,
    payload_written: usize,
}

impl Memory {
    fn new() -> Self {
        Self {
            bytes: vec![0; 128 * 1024],
            read_only: None,
            used_publications: 0,
            flags_on_publication: None,
            payload_read: core::cell::Cell::new(0),
            payload_written: 0,
        }
    }

    fn range(&self, address: u64, len: usize) -> Option<std::ops::Range<usize>> {
        let start = usize::try_from(address.checked_sub(BASE)?).ok()?;
        let end = start.checked_add(len)?;
        (end <= self.bytes.len()).then_some(start..end)
    }

    // Fixture writes model CPU-owned guest writes; permission policy below
    // applies only to device DMA, not to the acceptance driver's setup code.
    fn put(&mut self, address: u64, bytes: &[u8]) {
        let range = self.range(address, bytes.len()).unwrap();
        self.bytes[range].copy_from_slice(bytes);
    }

    fn bytes(&self, address: u64, len: usize) -> &[u8] {
        &self.bytes[self.range(address, len).unwrap()]
    }

    fn word(&self, address: u64) -> u16 {
        u16::from_le_bytes(self.bytes(address, 2).try_into().unwrap())
    }

    fn dword(&self, address: u64) -> u32 {
        u32::from_le_bytes(self.bytes(address, 4).try_into().unwrap())
    }

    fn desc(&mut self, index: u16, address: u64, len: u32, flags: u16, next: u16) {
        let slot = DESC + u64::from(index) * 16;
        self.put(slot, &address.to_le_bytes());
        self.put(slot + 8, &len.to_le_bytes());
        self.put(slot + 12, &flags.to_le_bytes());
        self.put(slot + 14, &next.to_le_bytes());
    }

    fn header(&mut self, address: u64, kind: u32, sector: u64) {
        self.put(address, &kind.to_le_bytes());
        self.put(address + 4, &[0; 4]);
        self.put(address + 8, &sector.to_le_bytes());
    }

    fn post(&mut self, head: u16) {
        let index = self.word(AVAIL + 2);
        self.put(
            AVAIL + 4 + u64::from(index % QUEUE_SIZE) * 2,
            &head.to_le_bytes(),
        );
        self.put(AVAIL + 2, &index.wrapping_add(1).to_le_bytes());
    }
}

impl GuestMemory for Memory {
    fn read_payload(&self, address: u64, out: &mut [u8]) -> bool {
        let ok = self.read(address, out);
        if ok {
            self.payload_read.set(self.payload_read.get() + out.len());
        }
        ok
    }
    fn write_payload(&mut self, address: u64, data: &[u8]) -> bool {
        let ok = self.write(address, data);
        if ok {
            self.payload_written += data.len();
        }
        ok
    }
    fn check(&self, address: u64, len: usize, write: bool) -> bool {
        if self.range(address, len).is_none() {
            return false;
        }
        if write && let Some((base, size)) = self.read_only {
            return address + len as u64 <= base || address >= base + size as u64;
        }
        true
    }

    fn read(&self, address: u64, out: &mut [u8]) -> bool {
        if !self.check(address, out.len(), false) {
            return false;
        }
        out.copy_from_slice(self.bytes(address, out.len()));
        true
    }

    fn write(&mut self, address: u64, data: &[u8]) -> bool {
        if !self.check(address, data.len(), true) {
            return false;
        }
        self.put(address, data);
        self.used_publications += usize::from(address == USED + 2);
        if address == USED + 2
            && let Some(flags) = self.flags_on_publication
        {
            self.put(AVAIL, &flags.to_le_bytes());
        }
        true
    }
}

struct Disk {
    sectors: u64,
    calls: usize,
    fail: bool,
    fail_after: Option<usize>,
    writes: Vec<(u64, Vec<u8>)>,
    flush_calls: usize,
    fail_flush: bool,
}

fn pattern(offset: u64) -> u8 {
    (offset.wrapping_mul(37) ^ (offset >> 8) ^ 0x5a) as u8
}

impl BlockBackend for Disk {
    fn sectors(&self) -> u64 {
        self.sectors
    }

    fn read(&mut self, _slot: usize, sector: u64, out: &mut [u8]) -> Poll<bool> {
        self.calls += 1;
        assert!(!out.is_empty() && out.len() % 512 == 0 && out.len() <= 65536);
        assert!(sector + (out.len() / 512) as u64 <= self.sectors);
        if self.fail || self.fail_after.is_some_and(|limit| self.calls > limit) {
            return Poll::Ready(false);
        }
        for (index, byte) in out.iter_mut().enumerate() {
            *byte = pattern(sector * 512 + index as u64);
        }
        for (start, data) in &self.writes {
            if *start == sector && data.len() == out.len() {
                out.copy_from_slice(data);
            }
        }
        Poll::Ready(true)
    }

    fn write(&mut self, _slot: usize, sector: u64, data: &[u8]) -> Poll<bool> {
        self.calls += 1;
        assert!(data.len() % 512 == 0 && data.len() <= 65536);
        assert!(sector + (data.len() / 512) as u64 <= self.sectors);
        if self.fail || self.fail_after.is_some_and(|limit| self.calls > limit) {
            return Poll::Ready(false);
        }
        self.writes.push((sector, data.to_vec()));
        Poll::Ready(true)
    }

    fn flush(&mut self, _slot: usize) -> Poll<bool> {
        self.flush_calls += 1;
        Poll::Ready(!self.fail_flush)
    }
}

fn write<B: AsMut<[u8]>>(device: &mut VirtioBlock<B>, offset: usize, value: u32) {
    assert_eq!(device.write_word(offset, value), Some(()));
}

fn read<B: AsMut<[u8]>>(device: &VirtioBlock<B>, offset: usize) -> u32 {
    device.read_word(offset).unwrap()
}

fn features<B: AsMut<[u8]>>(device: &mut VirtioBlock<B>) -> [u32; 2] {
    write(device, 0x14, 0);
    let low = read(device, 0x10);
    write(device, 0x14, 1);
    let high = read(device, 0x10);
    [low, high]
}

fn negotiate<B: AsMut<[u8]>>(device: &mut VirtioBlock<B>, supported: [u32; 2]) {
    // Keep the pre-existing suite on the non-EVENT_IDX compatibility path.
    negotiate_features(device, [supported[0] & !(1 << 29), supported[1]]);
}

fn negotiate_features<B: AsMut<[u8]>>(device: &mut VirtioBlock<B>, supported: [u32; 2]) {
    write(device, 0x70, 1);
    write(device, 0x70, 3);
    for (selector, word) in supported.into_iter().enumerate() {
        write(device, 0x24, selector as u32);
        write(device, 0x20, word);
    }
    write(device, 0x70, 11);
}

fn queue<B: AsMut<[u8]>>(device: &mut VirtioBlock<B>, desc: u64, avail: u64, used: u64) {
    write(device, 0x30, 0);
    assert_eq!(read(device, 0x34), u32::from(QUEUE_SIZE));
    write(device, 0x38, u32::from(QUEUE_SIZE));
    for (offset, address) in [(0x80, desc), (0x90, avail), (0xa0, used)] {
        write(device, offset, address as u32);
        write(device, offset + 4, (address >> 32) as u32);
    }
    write(device, 0x44, 1);
    write(device, 0x70, 15);
}

struct Fixture {
    device: VirtioBlock,
    memory: Memory,
    disk: Disk,
}

impl Fixture {
    fn new() -> Self {
        Self::with_capacity(128)
    }

    fn with_capacity(sectors: u64) -> Self {
        let mut device = VirtioBlock::new(sectors);
        assert_eq!(read(&device, 0), 0x7472_6976);
        assert_eq!(read(&device, 4), 2);
        assert_eq!(read(&device, 8), 2);
        let advertised = features(&mut device);
        assert_ne!(advertised[0] & (1 << 5), 0, "read-only feature");
        assert_ne!(advertised[1] & 1, 0, "modern transport feature");
        assert_eq!(advertised[0] & (1 << 28), 0);
        assert_ne!(advertised[0] & (1 << 29), 0);
        negotiate(&mut device, advertised);
        assert_ne!(read(&device, 0x70) & 8, 0);
        queue(&mut device, DESC, AVAIL, USED);
        Self {
            device,
            memory: Memory::new(),
            disk: Disk {
                sectors,
                calls: 0,
                fail: false,
                fail_after: None,
                writes: Vec::new(),
                flush_calls: 0,
                fail_flush: false,
            },
        }
    }

    fn request(&mut self, kind: u32, sector: u64) {
        self.memory.header(HEADER, kind, sector);
        self.memory.desc(0, HEADER, 16, NEXT, 1);
        self.memory
            .desc(1, DATA, 512, NEXT | if kind == 1 { 0 } else { WRITE }, 2);
        self.memory.desc(2, STATUS, 1, WRITE, 0);
        self.memory.put(STATUS, &[0xff]);
        self.memory.post(0);
        write(&mut self.device, 0x50, 0);
    }

    fn service(&mut self, budget: usize) -> usize {
        self.device
            .service(&mut self.memory, &mut self.disk, budget)
    }

    fn complete(&self, index: u16, head: u32, len: u32, status: u8) {
        assert_eq!(self.memory.word(USED + 2), index.wrapping_add(1));
        let slot = USED + 4 + u64::from(index % QUEUE_SIZE) * 8;
        assert_eq!(self.memory.dword(slot), head);
        assert_eq!(self.memory.dword(slot + 4), len);
        assert_eq!(self.memory.bytes(STATUS, 1), [status]);
    }
}

fn writable(flush: bool) -> Fixture {
    let mut f = Fixture::new();
    f.device = VirtioBlock::with_access(128, true);
    let mut offered = features(&mut f.device);
    assert_eq!(offered[0] & (1 << 5), 0);
    assert_ne!(offered[0] & (1 << 9), 0);
    if !flush {
        offered[0] &= !(1 << 9);
    }
    negotiate(&mut f.device, offered);
    queue(&mut f.device, DESC, AVAIL, USED);
    f
}

fn post_flush(f: &mut Fixture) {
    f.memory.header(HEADER, 4, u64::MAX); // FLUSH ignores sector, transfers no data.
    f.memory.desc(0, HEADER, 16, NEXT, 1);
    f.memory.desc(1, STATUS, 1, WRITE, 0);
    f.memory.put(STATUS, &[0xff]);
    f.memory.post(0);
    write(&mut f.device, 0x50, 0);
}

mod async_checks;
mod concurrent_checks;
mod event_checks;
mod limits;
mod requests;
