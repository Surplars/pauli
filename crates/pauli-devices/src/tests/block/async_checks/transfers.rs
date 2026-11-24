//! Delayed backend progress, reset and completion assertions.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

use super::*;

#[test]
fn large_batches() {
    for (batch, buffer, expected) in [
        (4096, 65536, 16),
        (16384, 65536, 4),
        (65536, 65536, 1),
        (65536, 4096, 16), // A small frontend still bounds a larger adapter.
    ] {
        let mut f = Async::large(batch, buffer);
        let data: Vec<_> = (0..65536).map(|i| pattern(i)).collect();
        f.memory.put(DATA, &data);
        f.post(0, 1, 0, data.len());
        f.post(19, 4, 0, 0);
        assert_eq!(f.service(), 0);
        f.pending(0, 0);
        if expected == 1 {
            // The full submitted write is immutable while native I/O waits.
            f.memory.put(DATA, &vec![0xee; data.len()]);
        }
        f.finish_one();
        assert_eq!(f.backend.disk.calls, expected);
        assert_eq!(f.backend.disk.flush_calls, 0);
        assert_eq!(f.memory.word(USED + 2), 1);
        assert_eq!(f.device.write_bytes, 65536);
        assert_eq!(
            f.backend
                .disk
                .writes
                .iter()
                .flat_map(|(_, bytes)| bytes.iter().copied())
                .collect::<Vec<_>>(),
            data
        );
        f.backend.ready = false;
        assert_eq!(f.service(), 0); // Flush is not an implicit data completion.
        f.pending(19, 1);
        assert_eq!(
            f.backend.events.last(),
            Some(&Event::Submit(Kind::Flush, 0, 0))
        );
        f.finish_one();
        assert_eq!(f.backend.disk.flush_calls, 1);
        assert_eq!(f.device.flushes, 1);
        f.memory.put(DATA, &vec![0xcc; data.len()]);
        f.post(0, 0, 0, data.len());
        assert_eq!(f.service(), 0);
        f.finish_one();
        assert_eq!(f.device.read_bytes, 65536);
        assert_eq!(f.memory.bytes(DATA, data.len()), data);
        assert_eq!(f.backend.disk.calls, 2 * expected);
    }

    // A large read completing after a frontend reset must drain into the
    // retained large staging buffer, never the old completion target/guest RAM.
    let mut f = Async::large(65536, 65536);
    f.post(0, 0, 0, 65536);
    assert_eq!(f.service(), 0);
    write(&mut f.device, 0x70, 0);
    f.memory = Memory::new();
    f.memory.put(DATA, &vec![0xcc; 65536]);
    f.backend.ready = true;
    assert_eq!(f.service(), 0);
    assert_eq!(f.memory.bytes(DATA, 65536), vec![0xcc; 65536]);
    assert_eq!(f.memory.word(USED + 2), 0);
    assert!(!f.device.irq_pending());
    assert!(f.backend.active.is_none());
    let offered = features(&mut f.device);
    negotiate(&mut f.device, offered);
    queue(&mut f.device, DESC, AVAIL, USED);
    f.post(0, 0, 0, 65536);
    assert_eq!(f.service(), 0);
    f.finish_one();
    assert_eq!(f.backend.disk.calls, 2);
    assert_eq!(f.device.completed, 1);
    println!(
        "  64 KiB SG batch: 16 -> 1 backend data submissions; delayed read/write, flush, small-buffer cap and reset drain PASS"
    );
}
#[test]
fn read_views() {
    let mut f = Async::large(65536, 65536);
    f.backend.views = true;
    f.device.buffer_mut().fill(0xcc);
    f.post(0, 0, 0, 65536);
    for _ in 0..4 {
        assert_eq!(f.service(), 0);
    }
    assert_eq!(f.backend.view_calls, 0);
    assert_eq!(f.memory.payload_written, 0);
    f.backend.ready = true;
    assert_eq!(f.service(), 1);
    assert_eq!(f.backend.view_calls, 1);
    assert_eq!(f.memory.payload_written, 65536);
    assert!(f.device.buffer_mut().iter().all(|byte| *byte == 0xcc));
    assert_eq!(
        f.memory.bytes(DATA, 65536),
        (0..65536).map(pattern).collect::<Vec<_>>()
    );

    // Queue reset discards the callback target. Late completion only drains
    // into host scratch, including when the old RAM has a new owner.
    let mut old = Async::new(true);
    old.backend.views = true;
    old.post(0, 0, 0, 512);
    assert_eq!(old.service(), 0);
    write(&mut old.device, 0x70, 0);
    old.memory = Memory::new();
    old.memory.put(DATA, &[0xee; 512]);
    old.backend.ready = true;
    assert_eq!(old.service(), 0);
    assert_eq!(old.backend.view_calls, 0);
    assert_eq!(old.memory.bytes(DATA, 512), [0xee; 512]);
    assert_eq!(old.memory.word(USED + 2), 0);

    let mut revoked = Async::new(true);
    revoked.backend.views = true;
    revoked.post(0, 0, 0, 512);
    assert_eq!(revoked.service(), 0);
    revoked.memory.read_only = Some((DATA, 512));
    revoked.backend.ready = true;
    assert_eq!(revoked.service(), 0);
    assert_ne!(read(&revoked.device, 0x70) & 64, 0);
    revoked.pending(0, 0);
    assert_eq!(revoked.memory.payload_written, 0);
    assert!(revoked.backend.active.is_none());

    let mut peer = Async::new(true);
    peer.backend.views = true;
    peer.post(0, 0, 1, 512);
    assert_eq!(peer.service(), 0);
    peer.backend.ready = true;
    assert_eq!(peer.service(), 1);
    assert_eq!(peer.backend.view_calls, 1);
    assert_eq!(old.memory.bytes(DATA, 512), [0xee; 512]);
    println!(
        "  scoped read views: no model staging, one 64 KiB SG copy, pending/reset drain, revoked destination and peer isolation PASS"
    );
}
