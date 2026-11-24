//! Delayed backend progress, reset and completion assertions.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

use super::*;

#[test]
fn ordered_flush() {
    let mut f = Async::new(true);
    f.memory.put(DATA, &vec![0x39; 4096]);
    f.memory.put(DATA + 5 * 4096, &vec![0x73; 4096]);
    f.post(0, 1, 0, 4096);
    f.post(3, 4, 0, 0);
    f.post(5, 1, 8, 4096);
    assert_eq!(f.service(), 0);
    for _ in 0..4 {
        assert_eq!(f.service(), 0);
    }
    f.pending(0, 0);
    assert!(!f.device.irq_pending());
    assert_eq!(f.backend.events, [Event::Submit(Kind::Write, 0, 4096)]);
    assert_eq!(
        f.memory.payload_read.get(),
        4096,
        "pending polls must not reread payload"
    );
    assert_eq!(
        f.memory.payload_written, 0,
        "ring/status writes are not payload"
    );
    // Device-owned metadata/payload is a snapshot, not a second guest fetch.
    f.memory.put(DATA, &vec![0xff; 4096]);
    f.memory.header(HEADER, 0, 123);
    f.memory.desc(1, 0x8800_0000, 4096, NEXT, 2);
    f.backend.ready = true;
    assert_eq!(f.service(), 1);
    assert_eq!(f.backend.disk.writes, [(0, vec![0x39; 4096])]);
    assert_eq!(f.service(), 0); // Flush begins only after OUT completed.
    f.backend.ready = false;
    assert_eq!(f.service(), 0);
    f.pending(3, 1);
    assert_eq!(f.backend.disk.writes.len(), 1); // Following OUT is not admitted.
    f.backend.ready = true;
    assert_eq!(f.service(), 1);
    assert_eq!(f.backend.disk.flush_calls, 1);
    assert_eq!(
        f.memory.payload_read.get(),
        4096,
        "flush has no data payload"
    );
    assert_eq!(f.service(), 0);
    assert_eq!(f.service(), 1);
    assert_eq!(
        f.backend.events,
        [
            Event::Submit(Kind::Write, 0, 4096),
            Event::Complete(Kind::Write),
            Event::Submit(Kind::Flush, 0, 0),
            Event::Complete(Kind::Flush),
            Event::Submit(Kind::Write, 8, 4096),
            Event::Complete(Kind::Write)
        ]
    );
    assert_eq!(f.device.completed, 3);
    assert_eq!(f.memory.payload_read.get(), 8192);
    assert_eq!(f.memory.payload_written, 0);
}
#[test]
fn writethrough_and_errors() {
    for fail in [false, true] {
        let mut f = Async::new(false);
        f.memory.put(DATA, &[0x51; 512]);
        f.backend.disk.fail_flush = fail;
        f.post(0, 1, 0, 512);
        assert_eq!(f.service(), 0);
        f.backend.ready = true;
        assert_eq!(f.service(), 0); // Data finished, durability is still pending.
        f.pending(0, 0);
        assert_eq!(
            f.backend.events.last(),
            Some(&Event::Submit(Kind::Flush, 0, 0))
        );
        assert_eq!(f.service(), 1);
        assert_eq!(f.memory.bytes(STATUS, 1), [u8::from(fail)]);
        assert_eq!(f.device.flushes, u64::from(!fail));
        assert_eq!(f.device.write_bytes, 512);
    }
    let mut f = Async::new(true);
    f.backend.disk.fail_after = Some(1);
    f.post(0, 0, 0, 8192);
    assert_eq!(f.service(), 0);
    f.backend.ready = true;
    assert_eq!(f.service(), 0);
    assert_eq!(f.service(), 1);
    assert_eq!(f.memory.dword(USED + 8), 4097); // Confirmed prefix plus IOERR status.
    assert_eq!(f.memory.bytes(STATUS, 1), [1]);
    assert_eq!(f.device.read_bytes, 4096);
    assert_eq!(
        f.memory.payload_written, 4096,
        "failed suffix/status is not copied payload"
    );
}
#[test]
fn reset_and_broken_queue() {
    let mut f = Async::new(true);
    assert!(!f.device.needs_service());
    f.post(0, 0, 1, 512);
    assert!(f.device.needs_service());
    assert_eq!(f.service(), 0);
    assert!(f.device.needs_service());
    write(&mut f.device, 0x70, 0);
    assert!(!f.device.needs_service() && f.backend.active.is_some());
    f.memory = Memory::new(); // Old guest completion targets have been reused.
    f.memory.put(DATA, &[0xcc; 512]);
    assert_eq!(f.service(), 0); // Reset does not wait or discard native ownership.
    assert_eq!(f.memory.bytes(DATA, 512), [0xcc; 512]);
    f.backend.ready = true;
    assert_eq!(f.service(), 0); // Drains into host scratch, never old guest RAM.
    assert_eq!(f.memory.bytes(DATA, 512), [0xcc; 512]);
    assert_eq!(f.memory.word(USED + 2), 0);
    assert!(f.backend.active.is_none());
    assert!(!f.device.needs_service());
    let offered = features(&mut f.device);
    negotiate(&mut f.device, offered);
    queue(&mut f.device, DESC, AVAIL, USED);
    f.post(0, 0, 9, 512);
    assert_eq!(f.service(), 0);
    assert_eq!(f.service(), 1);
    assert_eq!(
        f.memory.bytes(DATA, 512),
        (0..512).map(|i| pattern(9 * 512 + i)).collect::<Vec<_>>()
    );
    assert_eq!(f.device.completed, 1);

    let mut f = Async::new(true);
    f.post(0, 0, 1, 512);
    assert_eq!(f.service(), 0);
    write(&mut f.device, 0x80, DESC as u32); // Illegal live queue reconfiguration.
    assert_ne!(read(&f.device, 0x70) & 64, 0);
    f.backend.ready = true;
    assert_eq!(f.service(), 0);
    f.pending(0, 0);
    assert!(f.backend.active.is_none());
}
#[test]
fn peer_and_batch_limits() {
    let mut revoked = Async::new(true);
    revoked.post(0, 0, 1, 512);
    assert_eq!(revoked.service(), 0);
    revoked.memory.read_only = Some((DATA, 512));
    revoked.backend.ready = true;
    assert_eq!(revoked.service(), 0);
    assert_ne!(read(&revoked.device, 0x70) & 64, 0);
    revoked.pending(0, 0);
    assert_eq!(revoked.memory.bytes(DATA, 512), [0; 512]);

    let mut slow = Async::new(true);
    let mut peer = Async::new(true);
    slow.post(0, 0, 1, 512);
    peer.post(0, 0, 9, 512);
    assert_eq!(slow.service(), 0);
    peer.backend.ready = true;
    assert_eq!(peer.service(), 0);
    assert_eq!(peer.service(), 1);
    slow.pending(0, 0);
    assert_eq!(slow.backend.disk.calls, 0);
    assert_ne!(slow.memory.bytes(DATA, 512), peer.memory.bytes(DATA, 512));
    slow.backend.ready = true;
    assert_eq!(slow.service(), 1);

    for limit in [512, 4096] {
        let mut f = Async::new(true);
        f.backend.limit = limit;
        f.backend.ready = true;
        f.post(0, 0, 0, 64 * 1024);
        for _ in 0..64 * 1024 / limit {
            assert_eq!(f.service(), 0);
        }
        assert_eq!(f.service(), 1);
        assert_eq!(f.backend.disk.calls, 64 * 1024 / limit);
        assert_eq!(f.memory.dword(USED + 8), 65537);
        assert_eq!(f.device.read_bytes, 65536);
    }
    let mut f = Async::new(true);
    f.backend.blocked = true;
    f.post(0, 1, 0, 512);
    assert_eq!(f.service(), 0);
    assert!(f.backend.active.is_none());
    assert!(f.backend.events.is_empty());
    f.backend.blocked = false;
    f.backend.ready = true;
    assert_eq!(f.service(), 0);
    assert_eq!(f.service(), 1);
    assert_eq!(f.backend.disk.calls, 1);
}
