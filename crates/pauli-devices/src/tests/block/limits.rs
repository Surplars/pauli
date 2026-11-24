//! Block request framing, durability and queue-boundary assertions.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

use super::*;

#[test]
fn bounded_transfer_and_partial_error() {
    let sector = 0x1_0000_0000;
    let mut f = Fixture::with_capacity(sector + 256);
    f.memory.header(HEADER, 0, sector);
    f.memory.desc(0, HEADER, 16, NEXT, 1);
    for index in 1..=16 {
        f.memory.desc(
            index,
            DATA + u64::from(index - 1) * 4096,
            4096,
            NEXT | WRITE,
            index + 1,
        );
    }
    f.memory.desc(17, STATUS, 1, WRITE, 0);
    f.memory.post(0);
    write(&mut f.device, 0x50, 0);
    assert_eq!(f.service(1), 1);
    f.complete(0, 0, 65_537, 0);
    assert_eq!(f.disk.calls, 16);
    assert_eq!(f.device.read_bytes, 65_536);
    for (index, byte) in f.memory.bytes(DATA, 65_536).iter().enumerate() {
        assert_eq!(*byte, pattern(sector * 512 + index as u64));
    }

    let mut f = Fixture::new();
    f.disk.fail_after = Some(1);
    f.memory.header(HEADER, 0, 1);
    f.memory.desc(0, HEADER, 16, NEXT, 1);
    f.memory.desc(1, DATA, 4096, NEXT | WRITE, 2);
    f.memory.desc(2, DATA + 4096, 4096, NEXT | WRITE, 3);
    f.memory.desc(3, STATUS, 1, WRITE, 0);
    f.memory.post(0);
    write(&mut f.device, 0x50, 0);
    assert_eq!(f.service(1), 1);
    f.complete(0, 0, 4097, 1);
    assert_eq!(f.disk.calls, 2);
    assert_eq!(f.device.read_bytes, 4096);
    assert_eq!(f.memory.bytes(DATA + 4096, 4096), &[0; 4096]);
    assert_eq!(read(&f.device, 0x70) & 64, 0);
    println!("  maximum transfer/u64 sector/partial backend error PASS");
}

#[test]
fn empty_segments() {
    let mut f = Fixture::new();
    f.memory.header(HEADER, 0, 128);
    f.memory.desc(0, HEADER, 0, NEXT, 1);
    f.memory.desc(1, HEADER, 16, NEXT, 2);
    f.memory.desc(2, DATA, 0, WRITE | NEXT, 3);
    f.memory.desc(3, STATUS, 1, WRITE, 0);
    f.memory.post(0);
    write(&mut f.device, 0x50, 0);
    assert_eq!(f.service(1), 1);
    f.complete(0, 0, 1, 0);
    assert_eq!(f.disk.calls, 0);
    println!("  empty segments and zero-data request PASS");
}

#[test]
fn features_and_reset() {
    let mut device = VirtioBlock::new(0x1_2345_6789);
    assert_eq!(read(&device, 0x100), 0x2345_6789);
    assert_eq!(read(&device, 0x104), 1);
    let advertised = features(&mut device);
    for bad in [
        [advertised[0] | (1 << 31), advertised[1]],
        [advertised[0], 0],
    ] {
        write(&mut device, 0x70, 0);
        negotiate(&mut device, bad);
        assert_eq!(read(&device, 0x70) & 8, 0);
    }
    let mut f = Fixture::new();
    f.request(0, 0);
    write(&mut f.device, 0x44, 0);
    assert_eq!(f.service(1), 0);
    assert_eq!(f.disk.calls, 0);
    write(&mut f.device, 0x70, 0);
    assert_eq!(read(&f.device, 0x70), 0);
    assert_eq!(read(&f.device, 0x44), 0);
    assert!(!f.device.irq_pending());
    assert_eq!(
        read(&f.device, 0x100),
        128,
        "reset retains immutable capacity"
    );
    f.memory = Memory::new();
    let advertised = features(&mut f.device);
    negotiate(&mut f.device, advertised);
    queue(&mut f.device, DESC, AVAIL, USED);
    f.request(0, 1);
    assert_eq!(f.service(1), 1);
    f.complete(0, 0, 513, 0);
    println!("  feature negotiation/capacity/reset PASS");
}

#[test]
fn service_budget() {
    let mut f = Fixture::new();
    for index in 0..8u16 {
        let header = HEADER + u64::from(index) * 32;
        let status = STATUS + u64::from(index);
        f.memory.header(header, 0, u64::from(index));
        f.memory.desc(index * 3, header, 16, NEXT, index * 3 + 1);
        f.memory.desc(
            index * 3 + 1,
            DATA + u64::from(index) * 512,
            512,
            NEXT | WRITE,
            index * 3 + 2,
        );
        f.memory.desc(index * 3 + 2, status, 1, WRITE, 0);
        f.memory.post(index * 3);
    }
    write(&mut f.device, 0x50, 0);
    assert_eq!(f.service(1), 1);
    assert_eq!(f.memory.word(USED + 2), 1);
    assert_eq!(f.service(usize::MAX), 4);
    assert_eq!(f.memory.word(USED + 2), 5);
    assert_eq!(f.service(4), 3);
    assert_eq!(f.memory.word(USED + 2), 8);
    assert_eq!(f.disk.calls, 8);
    assert_eq!(f.service(1), 0);
    println!("  bounded work and deferred backlog PASS");
}

#[test]
fn index_wrap() {
    let mut f = Fixture::new();
    f.memory.header(HEADER, 0xfeed, 0);
    f.memory.desc(0, HEADER, 16, NEXT, 1);
    f.memory.desc(1, STATUS, 1, WRITE, 0);
    // Complete each request before reusing its descriptors. Advancing a full
    // 16-bit epoch catches avail/used arithmetic which only handles ring wrap.
    for index in 0..65_539u32 {
        f.memory.put(STATUS, &[0xff]);
        f.memory.post(0);
        write(&mut f.device, 0x50, 0);
        assert_eq!(f.service(1), 1);
        f.complete(index as u16, 0, 1, 2);
    }
    assert_eq!(f.disk.calls, 0);
    assert_eq!(f.memory.word(USED + 2), 3);
    println!("  16-bit avail/used epoch wrap PASS");
}
