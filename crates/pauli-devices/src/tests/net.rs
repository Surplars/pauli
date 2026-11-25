//! Private split rings and copied-frame link lifecycle checks.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

//! Production VirtIO-net + isolated link, fake private RAM. No QEMU, host NIC,
//! disk or native DMA. Equal GPAs intentionally name different VM allocations.
use super::*;
use crate::{
    memory::GuestMemory,
    net_link::{DEPTH, Endpoint, Error, Link},
    virtio_net::{FRAME_MAX, HEADER, NetBackend, VirtioNet},
};

// Equal mock GPAs intentionally refer to two different memory allocations.
const BASE: u64 = 0x4000_0000;
const RX: u64 = BASE + 0x8000;
const TX: u64 = BASE + 0xa000;
const N: u16 = 8;
struct Memory {
    bytes: Vec<u8>,
    readonly: bool,
}
impl Memory {
    fn new() -> Self {
        Self {
            bytes: vec![0; 128 * 1024],
            readonly: false,
        }
    }
    fn range(&self, addr: u64, len: usize) -> Option<std::ops::Range<usize>> {
        let start = usize::try_from(addr.checked_sub(BASE)?).ok()?;
        let end = start.checked_add(len)?;
        (end <= self.bytes.len()).then_some(start..end)
    }
    fn put(&mut self, addr: u64, bytes: &[u8]) {
        let range = self.range(addr, bytes.len()).unwrap();
        self.bytes[range].copy_from_slice(bytes);
    }
    fn u16(&self, addr: u64) -> u16 {
        u16::from_le_bytes(self.bytes[self.range(addr, 2).unwrap()].try_into().unwrap())
    }
    fn u32(&self, addr: u64) -> u32 {
        u32::from_le_bytes(self.bytes[self.range(addr, 4).unwrap()].try_into().unwrap())
    }
    fn desc(&mut self, direction: usize, addr: u64, len: usize, flags: u16, next: u16) {
        let desc = BASE + 0x1000 + direction as u64 * 0x3000;
        self.put(desc, &addr.to_le_bytes());
        self.put(desc + 8, &(len as u32).to_le_bytes());
        self.put(desc + 12, &flags.to_le_bytes());
        self.put(desc + 14, &next.to_le_bytes());
    }
    fn post(&mut self, direction: usize) {
        let avail = BASE + 0x2000 + direction as u64 * 0x3000;
        let idx = self.u16(avail + 2);
        self.put(avail + 4 + 2 * u64::from(idx % N), &0u16.to_le_bytes());
        self.put(avail + 2, &idx.wrapping_add(1).to_le_bytes());
    }
}
impl GuestMemory for Memory {
    fn check(&self, addr: u64, len: usize, write: bool) -> bool {
        self.range(addr, len).is_some() && !(write && self.readonly && addr == RX)
    }
    fn read(&self, addr: u64, out: &mut [u8]) -> bool {
        let Some(range) = self.range(addr, out.len()) else {
            return false;
        };
        out.copy_from_slice(&self.bytes[range]);
        true
    }
    fn write(&mut self, addr: u64, data: &[u8]) -> bool {
        if !self.check(addr, data.len(), true) {
            return false;
        }
        self.put(addr, data);
        true
    }
}
fn setup(device: &mut VirtioNet) {
    for (offset, value) in [
        (0x70, 1),
        (0x70, 3),
        (0x24, 0),
        (0x20, (1 << 5) | (1 << 16)),
        (0x24, 1),
        (0x20, 1),
        (0x70, 11),
    ] {
        device.write_word(offset, value).unwrap();
    }
    for direction in 0..2 {
        let base = 0x1000 + direction * 0x3000;
        for (offset, value) in [
            (0x30, direction),
            (0x38, u32::from(N)),
            (0x80, BASE as u32 + base),
            (0x90, BASE as u32 + base + 0x1000),
            (0xa0, BASE as u32 + base + 0x2000),
            (0x44, 1),
        ] {
            device.write_word(offset, value).unwrap();
        }
    }
    device.write_word(0x70, 15).unwrap();
}
fn packet(seed: u8) -> [u8; 60] {
    let mut data = [seed; 60];
    data[12] = 0x88;
    data[13] = 0xb5;
    data
}

#[test]
fn a_committed_batch_is_counted_once_and_cannot_outlive_either_port_or_reset() {
    assert_eq!(DEPTH, 16);
    let mut link = Link::new();
    let a = link.bind(0, 1, 1).unwrap();
    link.activate(a).unwrap();
    let mut sender = link.backend(a).unwrap();
    sender.transmit(&packet(0)).unwrap();
    assert_eq!(sender.ready(), None);
    let b = link.bind(1, 2, 0).unwrap();
    link.activate(b).unwrap();
    let mut sender = link.backend(a).unwrap();
    for seed in 0..=16 {
        sender.transmit(&packet(seed)).unwrap();
    }
    let hint = sender.ready().unwrap();
    assert_eq!((hint.slot(), hint.epoch()), (1, 2));
    let sender = link.snapshot(a).unwrap();
    assert_eq!(
        (sender.tx, sender.full, sender.down, sender.dropped),
        (16, 1, 1, 2)
    );
    assert_eq!(link.snapshot(b).unwrap().queued, 16);
    let mut out = [0; FRAME_MAX];
    for i in 0..16 {
        assert_eq!(
            link.receiver(b, hint).unwrap().receive(&mut out),
            Ok(Some(60))
        );
        assert_eq!(&out[..60], &packet(i));
    }
    assert!(link.receiver(b, hint).is_none());
    assert_eq!(link.snapshot(b).unwrap().rx, 16);
    let mut sender = link.backend(a).unwrap();
    sender.transmit(&packet(17)).unwrap();
    let before_reset = sender.ready().unwrap();
    link.backend(b).unwrap().transmit(&packet(18)).unwrap();
    link.reset(a).unwrap();
    assert_eq!(link.snapshot(a).unwrap().queued, 0);
    link.backend(a).unwrap().transmit(&packet(19)).unwrap();
    assert!(link.receiver(b, before_reset).is_none());
    let mut sender = link.backend(a).unwrap();
    sender.transmit(&packet(20)).unwrap();
    let before_restart = sender.ready().unwrap();
    link.retire(a).unwrap();
    let fresh = link.bind(0, 3, 1).unwrap();
    link.activate(fresh).unwrap();
    assert_eq!(link.snapshot(fresh).unwrap().tx, 0);
    let mut sender = link.backend(fresh).unwrap();
    sender.transmit(&packet(21)).unwrap();
    let current = sender.ready().unwrap();
    assert!(link.receiver(b, before_restart).is_none());
    assert_eq!(
        link.receiver(b, current).unwrap().receive(&mut out),
        Ok(Some(60))
    );
    assert_eq!(&out[..60], &packet(21));
    link.retire(b).unwrap();
    let replacement = link.bind(1, 4, 0).unwrap();
    link.activate(replacement).unwrap();
    link.backend(fresh).unwrap().transmit(&packet(22)).unwrap();
    assert!(link.receiver(replacement, current).is_none());
}
fn tx(mem: &mut Memory, data: &[u8]) {
    mem.put(TX, &[0; HEADER]);
    mem.put(TX + HEADER as u64, data);
    mem.desc(1, TX, HEADER + data.len(), 0, 0);
    mem.post(1);
}
fn rx(mem: &mut Memory) {
    mem.desc(0, RX, HEADER + FRAME_MAX, 2, 0);
    mem.post(0);
}
fn reset(device: &mut VirtioNet, memory: &mut Memory) {
    device.write_word(0x70, 0).unwrap();
    *memory = Memory::new();
    setup(device);
}
#[test]
fn queues_and_lifecycle() {
    let (mut a, mut b) = (
        VirtioNet::new([2, 0, 0, 0, 0, 1]),
        VirtioNet::new([2, 0, 0, 0, 0, 2]),
    );
    let (mut am, mut bm) = (Memory::new(), Memory::new());
    setup(&mut a);
    setup(&mut b);
    assert_eq!(a.read_word(8), Some(1));
    assert_eq!(a.read_config_byte(6), Some(0));
    a.set_link(true);
    assert_eq!(a.read_word(0x60), Some(2));
    a.write_word(0x64, 2).unwrap();
    assert!(!a.irq_pending());
    a.set_link(false);
    assert!(a.irq_pending());
    a.write_word(0x64, 2).unwrap();
    let mut link = Link::new();
    let ae = link.bind(0, 10, 1).unwrap();
    let be = link.bind(1, 20, 0).unwrap();
    link.activate(ae).unwrap();
    link.activate(be).unwrap();
    a.set_link(true);
    b.set_link(true);
    a.write_word(0x64, 3).unwrap();
    b.write_word(0x64, 3).unwrap();
    rx(&mut bm);
    tx(&mut am, &packet(7));
    am.put(TX + 10, &[0xa5, 0x5a]); // Unused TX num_buffers is not offload state.
    am.put(BASE + 0x5000, &1u16.to_le_bytes()); // Suppress TX completion IRQ only.
    a.service(&mut am, &mut link.backend(ae).unwrap(), 4);
    assert_eq!(am.u16(BASE + 0x6002), 1);
    assert_eq!(am.u32(BASE + 0x6008), 0);
    assert!(!a.irq_pending());
    assert!(!b.irq_pending());
    b.service(&mut bm, &mut link.backend(be).unwrap(), 4);
    assert_eq!(bm.u16(BASE + 0x3002), 1);
    assert_eq!(bm.u32(BASE + 0x3008), (HEADER + 60) as u32);
    assert_eq!(
        &bm.bytes[bm.range(RX + HEADER as u64, 60).unwrap()],
        &packet(7)
    );
    assert_eq!(bm.u16(RX + 10), 1);
    assert!(b.irq_pending());
    b.write_word(0x64, 1).unwrap();
    assert!(!b.irq_pending());
    println!("  real split queues, copied frames, independent IRQ/ACK and modern RX header PASS");

    let fresh = link_lifecycle(&mut link, ae, be);
    malformed_and_wrap(a, b, am, bm, link, fresh, be);
}

fn link_lifecycle(link: &mut Link, ae: Endpoint, be: Endpoint) -> Endpoint {
    for seed in 0..DEPTH + 2 {
        link.backend(ae)
            .unwrap()
            .transmit(&packet(seed as u8))
            .unwrap();
    }
    assert_eq!(link.snapshot(be).unwrap().queued, DEPTH);
    assert_eq!(link.snapshot(ae).unwrap().dropped, 2);
    link.backend(be).unwrap().transmit(&packet(99)).unwrap();
    link.reset(ae).unwrap();
    assert_eq!(link.snapshot(ae).unwrap().queued, 0);
    assert_eq!(link.snapshot(be).unwrap().queued, 0);
    link.backend(ae).unwrap().transmit(&packet(11)).unwrap();
    link.backend(be).unwrap().transmit(&packet(12)).unwrap();
    link.retire(ae).unwrap();
    assert!(!link.snapshot(be).unwrap().up);
    assert_eq!(link.snapshot(be).unwrap().queued, 0);
    link.backend(be).unwrap().transmit(&packet(13)).unwrap(); // Offline peer is loss, never retained.
    let fresh = link.bind(0, 11, 1).unwrap();
    link.activate(fresh).unwrap();
    let mut output = [0; FRAME_MAX];
    assert_eq!(
        link.backend(fresh).unwrap().receive(&mut output).unwrap(),
        None
    );
    assert!(matches!(link.backend(ae), Err(Error::Stale)));
    assert_eq!(link.retire(ae), Err(Error::Stale));
    assert_eq!(link.reset(ae), Err(Error::Stale));
    assert_eq!(link.bind(2, 30, 1), Err(Error::Invalid)); // No stealing a live pair.
    let ce = link.bind(2, 30, 3).unwrap();
    let de = link.bind(3, 40, 2).unwrap();
    link.activate(ce).unwrap();
    link.activate(de).unwrap();
    link.backend(ce).unwrap().transmit(&packet(44)).unwrap();
    assert_eq!(
        link.backend(fresh).unwrap().receive(&mut output).unwrap(),
        None
    );
    assert_eq!(
        link.backend(be).unwrap().receive(&mut output).unwrap(),
        None
    );
    assert_eq!(
        link.backend(de).unwrap().receive(&mut output).unwrap(),
        Some(60)
    );
    println!(
        "  bounded loss, two-direction reset, offline/restart epoch isolation and distinct link pairs PASS"
    );

    fresh
}

fn malformed_and_wrap(
    mut a: VirtioNet,
    mut b: VirtioNet,
    mut am: Memory,
    mut bm: Memory,
    mut link: Link,
    fresh: Endpoint,
    be: Endpoint,
) {
    for fault in 0..7 {
        reset(&mut a, &mut am);
        tx(&mut am, &packet(1));
        match fault {
            0 => am.desc(1, BASE + 0x40000, HEADER + 60, 0, 0),
            1 => am.desc(1, TX, HEADER + 60, 1, 0), // Descriptor cycle.
            2 => am.desc(1, TX, HEADER + 60, 2, 0), // Wrong direction.
            3 => am.desc(1, BASE + 0x1000, HEADER + 60, 0, 0), // Other queue metadata.
            4 => am.desc(1, TX, HEADER + 60, 4, 0), // Unnegotiated indirect.
            5 => am.put(TX, &[1]),                  // No checksum offload negotiated.
            _ => am.put(BASE + 0x5002, &(N + 1).to_le_bytes()), // Avail overrun.
        }
        let before = link.snapshot(be).unwrap().queued;
        a.service(&mut am, &mut link.backend(fresh).unwrap(), 4);
        assert_ne!(a.read_word(0x70).unwrap() & 64, 0);
        assert_eq!(link.snapshot(be).unwrap().queued, before);
    }
    reset(&mut b, &mut bm);
    rx(&mut bm);
    bm.readonly = true;
    link.backend(fresh).unwrap().transmit(&packet(55)).unwrap();
    b.service(&mut bm, &mut link.backend(be).unwrap(), 4);
    assert_ne!(b.read_word(0x70).unwrap() & 64, 0);
    assert_eq!(link.snapshot(be).unwrap().queued, 1); // Reject DMA before dequeue.
    assert_eq!(bm.u16(BASE + 0x3002), 0);
    reset(&mut b, &mut bm);
    link.reset(be).unwrap();
    assert_eq!(b.read_word(0x60), Some(0));
    assert_eq!(b.read_word(0x44), Some(1));
    println!(
        "  foreign/readonly RAM, cross-queue alias, cycles, flags, offload and avail-overrun rejection PASS"
    );

    reset(&mut a, &mut am);
    am.put(BASE + 0x5000, &1u16.to_le_bytes());
    for _ in 0..=u16::MAX {
        tx(&mut am, &packet(66));
        a.service(&mut am, &mut link.backend(fresh).unwrap(), 1);
        assert_eq!(a.read_word(0x70).unwrap() & 64, 0);
    }
    assert_eq!(am.u16(BASE + 0x6002), 0);
    a.write_word(0x70, 0).unwrap();
    link.reset(fresh).unwrap();
    assert_eq!(a.read_word(0x44), Some(0));
    assert_eq!(a.read_word(0x60), Some(0));
    println!(
        "VirtIO net offline PASS: 16-bit wrap, reset/stop/buffer/IRQ lifecycle (no host NIC or QEMU)"
    );
}
