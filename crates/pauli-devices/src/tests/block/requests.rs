//! Block request framing, durability and queue-boundary assertions.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

use super::*;

#[test]
fn writes_and_flush() {
    let mut f = writable(true);
    let data: Vec<u8> = (0..512).map(|i| pattern(i + 17)).collect();
    f.memory.put(DATA, &data);
    f.request(1, 7);
    assert_eq!(f.service(1), 1);
    f.complete(0, 0, 1, 0); // OUT used length counts status only.
    assert_eq!(f.disk.writes, [(7, data.clone())]);
    assert_eq!(f.disk.flush_calls, 0);
    f.request(0, 7);
    assert_eq!(f.service(1), 1);
    f.complete(1, 0, 513, 0);
    assert_eq!(f.memory.bytes(DATA, 512), data);
    post_flush(&mut f);
    assert_eq!(f.service(1), 1);
    f.complete(2, 0, 1, 0);
    assert_eq!(f.device.write_bytes, 512);
    assert_eq!(f.device.flushes, 1);
    f.disk.fail_flush = true;
    post_flush(&mut f);
    assert_eq!(f.service(1), 1);
    f.complete(3, 0, 1, 1);
    assert_eq!(f.device.flushes, 1, "failed flush never counts as durable");
    write(&mut f.device, 0x70, 0);
    assert_eq!(
        features(&mut f.device)[0] & (1 << 5),
        0,
        "reset retains assignment"
    );
}

#[test]
fn stable_writes_and_guards() {
    let data: Vec<u8> = (0..512).map(|i| pattern(i + 17)).collect();
    for fail in [false, true] {
        let mut f = writable(false);
        f.disk.fail_flush = fail;
        f.request(1, 0);
        assert_eq!(f.service(1), 1);
        f.complete(0, 0, 1, u8::from(fail));
        assert_eq!(
            f.disk.flush_calls, 1,
            "unnegotiated FLUSH requires stable writes"
        );
        post_flush(&mut f);
        assert_eq!(f.service(1), 1);
        f.complete(1, 0, 1, 2); // Never accept unnegotiated flush.
        assert_eq!(f.disk.flush_calls, 1);
    }
    let mut f = writable(true);
    f.request(1, 128);
    assert_eq!(f.service(1), 1);
    f.complete(0, 0, 1, 1);
    assert!(f.disk.writes.is_empty());
    let mut f = writable(true);
    f.request(1, 0);
    f.memory.desc(1, 0x8800_0000, 512, NEXT, 2);
    assert_eq!(f.service(1), 0);
    assert!(f.disk.writes.is_empty());
    assert_ne!(read(&f.device, 0x70) & 64, 0);
    let mut f = writable(true);
    f.request(4, 0); // A flush with a payload is malformed.
    assert_eq!(f.service(1), 0);
    assert_eq!(f.disk.flush_calls, 0);
    let mut f = writable(true);
    f.memory.header(HEADER, 1, 4);
    f.memory.put(HEADER + 16, &data);
    f.memory.read_only = Some((HEADER, 528));
    f.memory.desc(0, HEADER, 528, NEXT, 1); // Header and OUT payload may share a descriptor.
    f.memory.desc(1, STATUS, 1, WRITE, 0);
    f.memory.post(0);
    write(&mut f.device, 0x50, 0);
    assert_eq!(f.service(1), 1);
    f.complete(0, 0, 1, 0);
    assert_eq!(f.disk.writes, [(4, data)]);
    let mut f = writable(true);
    f.disk.fail_after = Some(1);
    f.request(1, 0);
    f.memory.desc(1, DATA, 4096, NEXT, 2);
    f.memory.desc(2, DATA + 4096, 4096, NEXT, 3);
    f.memory.desc(3, STATUS, 1, WRITE, 0);
    assert_eq!(f.service(1), 1);
    f.complete(0, 0, 1, 1); // A completed prefix never turns an I/O error into success.
    assert_eq!(f.disk.writes.len(), 1);
    assert_eq!(f.device.write_bytes, 4096);
    println!(
        "  writes/readback, real flush/error, stable-write fallback and assignment guards PASS"
    );
}

#[test]
fn reads_and_interrupts() {
    let mut f = Fixture::new();
    assert!(!f.device.needs_service());
    f.request(0, 3);
    assert_eq!(f.disk.calls, 0, "MMIO notify must not run backend I/O");
    assert_eq!(f.memory.word(USED + 2), 0);
    assert_eq!(f.service(0), 0);
    assert_eq!(f.service(1), 1);
    assert!(!f.device.needs_service());
    f.complete(0, 0, 513, 0);
    for (index, byte) in f.memory.bytes(DATA, 512).iter().enumerate() {
        assert_eq!(*byte, pattern(3 * 512 + index as u64));
    }
    assert!(f.device.irq_pending());
    assert_eq!(read(&f.device, 0x60) & 1, 1);
    write(&mut f.device, 0x64, 1);
    assert!(!f.device.irq_pending());
    assert_eq!(f.service(1), 0);

    // Without EVENT_IDX, avail.flags bit 0 suppresses used interrupts only;
    // it must not suppress used-ring publication or descriptor completion.
    f.memory.put(AVAIL, &1u16.to_le_bytes());
    f.request(0, 4);
    assert_eq!(f.service(1), 1);
    f.complete(1, 0, 513, 0);
    assert!(!f.device.irq_pending());
    println!("  read/used/interrupt/notify deferral PASS");
}

#[test]
fn scatter_gather() {
    let mut f = Fixture::new();
    f.memory.header(HEADER, 0, 2);
    // Header split across two readable descriptors; a five-byte writable
    // segment followed by 507 data bytes and the status byte in one segment.
    f.memory.desc(0, HEADER, 7, NEXT, 1);
    f.memory.desc(1, HEADER + 7, 9, NEXT, 2);
    f.memory.desc(2, DATA + 1, 5, WRITE | NEXT, 3);
    f.memory.desc(3, DATA + 0x101, 508, WRITE, 0);
    f.memory.post(0);
    write(&mut f.device, 0x50, 0);
    assert_eq!(f.service(1), 1);
    assert_eq!(f.memory.word(USED + 2), 1);
    assert_eq!(f.memory.dword(USED + 8), 513);
    for index in 0..512 {
        let address = if index < 5 {
            DATA + 1 + index
        } else {
            DATA + 0x101 + index - 5
        };
        assert_eq!(f.memory.bytes(address, 1)[0], pattern(2 * 512 + index));
    }
    assert_eq!(f.memory.bytes(DATA + 0x101 + 507, 1), [0]);
    println!("  byte-granular scatter/gather and stream framing PASS");
}

#[test]
fn ordinary_errors() {
    for (kind, sector, expected) in [(1, 0, 1), (0xfeed, 0, 2), (0, 128, 1), (0, u64::MAX, 1)] {
        let mut f = Fixture::new();
        f.request(kind, sector);
        assert_eq!(f.service(1), 1);
        f.complete(0, 0, 1, expected);
        assert_eq!(f.disk.calls, 0);
        assert_eq!(read(&f.device, 0x70) & 64, 0);
    }
    let mut f = Fixture::new();
    f.disk.fail = true;
    f.request(0, 0);
    assert_eq!(f.service(1), 1);
    f.complete(0, 0, 1, 1);
    assert_eq!(f.disk.calls, 1);
    assert_eq!(read(&f.device, 0x70) & 64, 0);
    println!("  read-only/out-of-range/unsupported/backend error PASS");
}

#[test]
fn malformed_requests() {
    let mutations: &[(&str, fn(&mut Fixture))] = &[
        ("host GPA", |f| {
            f.memory.desc(1, 0x8800_0000, 512, NEXT | WRITE, 2)
        }),
        ("overflow GPA", |f| {
            f.memory.desc(1, u64::MAX - 7, 512, NEXT | WRITE, 2)
        }),
        ("oversized DMA", |f| {
            f.memory.desc(1, DATA, u32::MAX, NEXT | WRITE, 2)
        }),
        ("write protection", |f| {
            f.memory.read_only = Some((DATA, 512))
        }),
        ("used ring protection", |f| {
            f.memory.read_only = Some((USED, 520))
        }),
        ("chain cycle", |f| {
            f.memory.desc(2, STATUS, 1, WRITE | NEXT, 1)
        }),
        ("indirect without feature", |f| {
            f.memory.desc(1, DATA, 512, INDIRECT, 0)
        }),
        ("unknown descriptor flags", |f| {
            f.memory.desc(1, DATA, 512, NEXT | WRITE | 8, 2)
        }),
        ("short header", |f| f.memory.desc(0, HEADER, 15, NEXT, 1)),
        ("readable data on read request", |f| {
            f.memory.desc(1, DATA, 512, NEXT, 2)
        }),
        ("writable header", |f| {
            f.memory.desc(0, HEADER, 16, NEXT | WRITE, 1)
        }),
        ("readable status after data", |f| {
            f.memory.desc(2, STATUS, 1, 0, 0)
        }),
        ("avail overrun", |f| {
            f.memory.put(AVAIL + 2, &(QUEUE_SIZE + 1).to_le_bytes())
        }),
        ("invalid chain index", |f| {
            f.memory.desc(0, HEADER, 16, NEXT, QUEUE_SIZE)
        }),
        ("segment exceeds SIZE_MAX", |f| {
            f.memory.desc(1, DATA, 8192, NEXT | WRITE, 2)
        }),
        ("segment count exceeds SEG_MAX", |f| {
            for index in 1..=17 {
                f.memory.desc(
                    index,
                    DATA + u64::from(index) * 512,
                    512,
                    NEXT | WRITE,
                    index + 1,
                );
            }
            f.memory.desc(18, STATUS, 1, WRITE, 0);
        }),
        ("payload aliases descriptor table", |f| {
            f.memory.desc(1, DESC, 512, NEXT | WRITE, 2)
        }),
    ];
    for (label, mutate) in mutations {
        let mut f = Fixture::new();
        f.request(0, 0);
        mutate(&mut f);
        f.service(1);
        assert_ne!(read(&f.device, 0x70) & 64, 0, "{label}");
        assert_eq!(read(&f.device, 0x60) & 2, 2, "{label}");
        assert!(f.device.irq_pending(), "{label}");
        assert_eq!(
            f.disk.calls, 0,
            "{label}: must validate the whole chain before I/O"
        );
        assert_eq!(f.service(1), 0, "{label}: broken queue cannot continue");
    }
    println!("  malformed rings/chains and GPA permissions PASS");
}

#[test]
fn queue_boundaries() {
    for (label, descriptors, available, used) in [
        ("descriptor high-word overflow", u64::MAX - 15, AVAIL, USED),
        ("host descriptors", 0x8800_0000, AVAIL, USED),
        ("host available ring", DESC, 0x8800_0000, USED),
        ("host used ring", DESC, AVAIL, 0x8800_0000),
        ("descriptor alignment", DESC + 1, AVAIL, USED),
        ("available alignment", DESC, AVAIL + 1, USED),
        ("used alignment", DESC, AVAIL, USED + 2),
        ("queue overlap", DESC, AVAIL, DESC),
    ] {
        let mut f = Fixture::new();
        write(&mut f.device, 0x44, 0);
        queue(&mut f.device, descriptors, available, used);
        f.request(0, 0);
        f.service(1);
        assert_ne!(read(&f.device, 0x70) & 64, 0, "{label}");
        assert_eq!(f.disk.calls, 0, "{label}");
    }
    let mut f = Fixture::new();
    write(&mut f.device, 0x38, 32);
    assert_ne!(read(&f.device, 0x70) & 64, 0, "live queue reconfiguration");
    println!("  queue GPA/overflow/alignment/layout ownership PASS");
}
