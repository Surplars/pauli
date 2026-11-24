//! Multi-slot ordering and notification race assertions.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

//! Real model checks for both EVENT_IDX directions. Interleaving hooks model
//! a guest publishing between the device's snapshot and notification rearm.
use super::*;
use concurrent_checks::Fixture4;

const USED_EVENT: u64 = AVAIL + 4 + 2 * QUEUE_SIZE as u64;
const AVAIL_EVENT: u64 = USED + 4 + 8 * QUEUE_SIZE as u64;

fn enable(device: &mut VirtioBlock) {
    write(device, 0x70, 0);
    let offered = features(device);
    negotiate_features(device, offered);
    queue(device, DESC, AVAIL, USED);
}

fn fixture() -> Fixture {
    let mut f = Fixture::new();
    enable(&mut f.device);
    f
}

fn pending() -> Fixture4 {
    let mut f = Fixture4::new(true);
    enable(&mut f.device);
    f
}

// Independent oracle: walk the short interval instead of repeating the
// production wrapping-subtraction formula.
fn crosses(event: u16, old: u16, new: u16) -> bool {
    (0..new.wrapping_sub(old)).any(|step| old.wrapping_add(step) == event)
}

fn post_with_kick(f: &mut Fixture, head: u16) -> bool {
    let old = f.memory.word(AVAIL + 2);
    f.memory.post(head);
    let kick = crosses(f.memory.word(AVAIL_EVENT), old, f.memory.word(AVAIL + 2));
    if kick {
        write(&mut f.device, 0x50, 0);
    }
    kick
}

#[test]
fn batch_and_epoch_wrap() {
    let mut f = fixture();
    for n in 0..4u16 {
        let header = HEADER + 16 * u64::from(n);
        f.memory.header(header, 0xfeed, 0); // Unsupported: no native I/O.
        f.memory.desc(2 * n, header, 16, NEXT, 2 * n + 1);
        f.memory.desc(2 * n + 1, STATUS + u64::from(n), 1, WRITE, 0);
    }
    // Offset the epoch so a four-completion batch straddles 0xffff -> 0.
    for _ in 0..2 {
        post_with_kick(&mut f, 0);
        assert_eq!(f.service(4), 1);
    }
    for batch in 0..16_385u32 {
        write(&mut f.device, 0x64, 1);
        let old = f.memory.word(USED + 2);
        let event = old.wrapping_add([u16::MAX, 0, 1, 3, 4][batch as usize % 5]);
        f.memory.put(USED_EVENT, &event.to_le_bytes());
        // EVENT_IDX ignores the old NO_INTERRUPT bit, even if a driver sets it.
        f.memory.put(AVAIL, &1u16.to_le_bytes());
        let kicks = (0..4).filter(|n| post_with_kick(&mut f, 2 * n)).count();
        assert_eq!(kicks, 1, "one notification covers the published batch");
        assert_eq!(f.service(4), 4);
        assert_eq!(f.memory.word(USED + 2), old.wrapping_add(4));
        assert_eq!(
            f.device.irq_pending(),
            crosses(event, old, old.wrapping_add(4))
        );
        assert_eq!(f.memory.word(AVAIL_EVENT), old.wrapping_add(4));
        assert_eq!(f.memory.word(USED), 0);
        assert!(!f.device.needs_service());
    }
    assert_eq!(f.disk.calls, 0);
    assert_eq!(f.device.notifications.kicks, 16_387);
    assert_eq!(f.device.notifications.checks, f.device.completion_batches);
    assert!(f.device.notifications.suppressed > 0);
}

struct RacingMemory {
    inner: Memory,
    event_on_publication: Option<u16>,
    post_on_arm: Option<bool>, // false: before event store; true: after it.
    denied: Option<u64>,
}
impl RacingMemory {
    fn new(inner: Memory) -> Self {
        Self {
            inner,
            event_on_publication: None,
            post_on_arm: None,
            denied: None,
        }
    }
}
impl GuestMemory for RacingMemory {
    fn check(&self, address: u64, len: usize, write: bool) -> bool {
        self.inner.check(address, len, write)
            && !self
                .denied
                .is_some_and(|at| address < at + 2 && at < address + len as u64)
    }
    fn read(&self, address: u64, out: &mut [u8]) -> bool {
        self.check(address, out.len(), false) && self.inner.read(address, out)
    }
    fn write(&mut self, address: u64, data: &[u8]) -> bool {
        if !self.check(address, data.len(), true) {
            return false;
        }
        let race = if address == AVAIL_EVENT {
            self.post_on_arm.take()
        } else {
            None
        };
        if race == Some(false) {
            self.inner.post(0);
        }
        if !self.inner.write(address, data) {
            return false;
        }
        if address == USED + 2
            && let Some(event) = self.event_on_publication.take()
        {
            self.inner.put(USED_EVENT, &event.to_le_bytes());
        }
        if race == Some(true) {
            self.inner.post(0);
        }
        true
    }
}

#[test]
fn publication_races() {
    for (before, after) in [(0u16, 1u16), (1, 0)] {
        let mut f = fixture();
        f.request(0, 0);
        f.memory.put(USED_EVENT, &before.to_le_bytes());
        let mut memory = RacingMemory::new(f.memory);
        memory.event_on_publication = Some(after);
        assert_eq!(f.device.service(&mut memory, &mut f.disk, 4), 1);
        assert_eq!(
            f.device.irq_pending(),
            after == 0,
            "sample event after used.idx"
        );
    }
    for after_store in [false, true] {
        let mut f = fixture();
        f.memory.header(HEADER, 0, 0);
        f.memory.desc(0, HEADER, 16, NEXT, 1);
        f.memory.desc(1, DATA, 512, NEXT | WRITE, 2);
        f.memory.desc(2, STATUS, 1, WRITE, 0);
        // A stale device-owned event must not prevent the initial scan/rearm.
        f.memory.put(AVAIL_EVENT, &123u16.to_le_bytes());
        let mut memory = RacingMemory::new(f.memory);
        memory.post_on_arm = Some(after_store);
        assert_eq!(f.device.service(&mut memory, &mut f.disk, 4), 0);
        assert!(
            f.device.needs_service(),
            "recheck must catch an un-kicked post"
        );
        assert_eq!(f.device.notifications.recheck_races, 1);
        assert_eq!(f.device.service(&mut memory, &mut f.disk, 4), 1);
        assert!(!f.device.needs_service());
        assert_eq!(memory.inner.word(AVAIL_EVENT), 1);
    }
}

#[test]
fn event_permissions_and_legacy() {
    for at in [USED_EVENT, AVAIL_EVENT] {
        let mut f = fixture();
        f.request(0, 0);
        let mut memory = RacingMemory::new(f.memory);
        memory.denied = Some(at);
        assert_eq!(f.device.service(&mut memory, &mut f.disk, 4), 0);
        assert_ne!(read(&f.device, 0x70) & 64, 0);
        assert_eq!(f.disk.calls, 0, "event fields belong to the validated ring");

        let mut f = Fixture::new(); // EVENT_IDX not negotiated.
        f.request(0, 0);
        let mut memory = RacingMemory::new(f.memory);
        memory.denied = Some(at);
        assert_eq!(f.device.service(&mut memory, &mut f.disk, 4), 1);
        assert!(
            f.device.irq_pending(),
            "legacy must not access event fields"
        );
    }
    let mut f = fixture();
    f.request(0, 0);
    f.memory.desc(2, AVAIL_EVENT, 1, WRITE, 0);
    assert_eq!(f.service(4), 0);
    assert_ne!(
        read(&f.device, 0x70) & 64,
        0,
        "payload may not alias event metadata"
    );
}

#[test]
fn reuse_without_consumption() {
    let mut f = pending();
    f.post(0, 0);
    f.service();
    assert_eq!(f.device.notifications.rearms, 1);
    for _ in 0..100 {
        assert_eq!(f.service(), 0);
    }
    assert_eq!(f.device.notifications.rearms, 1);
    assert_eq!(f.device.notifications.reuses, 100);
    assert_eq!(f.memory.word(AVAIL_EVENT), 1);
    assert!(crosses(f.memory.word(AVAIL_EVENT), 1, 2));
    f.post(1, 0); // A new post must still kick while the first I/O is pending.
    f.service();
    assert_eq!(f.backend.starts.len(), 2);
    assert_eq!(f.device.notifications.rearms, 2);
    f.backend.ready = 3;
    assert_eq!(f.service(), 2);
    assert_eq!(f.device.notifications.rearms, 2);
    for _ in 0..3 {
        write(&mut f.device, 0x50, 0); // A spurious kick does not move the index.
        f.service();
    }
    assert_eq!(f.device.notifications.rearms, 2);

    // Retaining an arm does not bypass trust-boundary ring validation.
    write(&mut f.device, 0x50, 0);
    let mut memory = RacingMemory::new(f.memory);
    memory.denied = Some(AVAIL_EVENT);
    assert_eq!(f.device.service(&mut memory, &mut f.backend, 4), 0);
    assert_ne!(read(&f.device, 0x70) & 64, 0);

    // Replacing a disabled queue at the SAME logical index needs publication.
    let mut f = fixture();
    f.service(4);
    assert_eq!(f.device.notifications.rearms, 1);
    write(&mut f.device, 0x44, 0);
    f.memory = Memory::new();
    f.memory.put(AVAIL_EVENT, &321u16.to_le_bytes());
    queue(&mut f.device, DESC, AVAIL, USED);
    f.service(4);
    assert_eq!(f.memory.word(AVAIL_EVENT), 0);
    assert_eq!(f.device.notifications.rearms, 2);

    // Keep backlog across a complete u16 wrap. Validity follows consumption,
    // not equality with an ancient cached index (both happen to be zero).
    f.memory.header(HEADER, 0xfeed, 0);
    f.memory.desc(0, HEADER, 16, NEXT, 1);
    f.memory.desc(1, STATUS, 1, WRITE, 0);
    post_with_kick(&mut f, 0);
    for _ in 0..u16::MAX {
        post_with_kick(&mut f, 0);
        assert_eq!(f.service(1), 1);
    }
    assert_eq!(f.device.notifications.rearms, 2);
    assert_eq!(f.service(1), 1);
    assert_eq!(f.device.notifications.rearms, 3);
    assert_eq!(f.memory.word(AVAIL_EVENT), 0);
}

#[test]
fn barriers_reset_and_peer() {
    let mut admitting = pending();
    admitting.post(0, 0);
    admitting.service();
    assert_eq!(admitting.memory.word(AVAIL_EVENT), 1);
    assert!(
        crosses(admitting.memory.word(AVAIL_EVENT), 1, 2),
        "pending I/O must still allow an immediate kick for the next request"
    );
    admitting.post(1, 0);
    admitting.service();
    assert_eq!(admitting.backend.starts.len(), 2);

    let mut f = pending();
    f.post(0, 1);
    f.post(1, 1);
    f.post(2, 4);
    f.post(3, 1);
    f.memory.put(USED_EVENT, &1u16.to_le_bytes());
    assert_eq!(f.service(), 0);
    assert_eq!(f.backend.starts.len(), 2);
    f.backend.ready = 2;
    assert_eq!(f.service(), 1);
    assert!(!f.device.irq_pending());
    assert_eq!(
        f.backend.starts.len(),
        2,
        "flush waits for both older writes"
    );
    f.backend.ready = 1;
    assert_eq!(f.service(), 1);
    assert!(f.device.irq_pending());
    f.service();
    assert_eq!(f.backend.starts.len(), 3);
    assert_eq!(f.backend.starts[2].1, 4);
    f.backend.ready = 1 << f.backend.starts[2].0;
    assert_eq!(f.service(), 1);
    assert_eq!(f.backend.disk.flush_calls, 1);
    assert_eq!(f.backend.starts[3].1, 1);
}

#[test]
fn reset_and_peer() {
    for reset in [0, 1, 2] {
        let mut old = pending();
        let mut peer = pending();
        for n in 0..4 {
            old.post(n, 0);
        }
        old.service();
        peer.post(0, 0);
        peer.service();
        match reset {
            0 => write(&mut old.device, 0x70, 0),
            1 => write(&mut old.device, 0x44, 0), // Queue disabled/stopped.
            _ => write(&mut old.device, 0x80, DESC as u32), // Malformed active queue.
        }
        old.memory = Memory::new();
        if reset == 0 {
            assert_eq!(old.device.notifications.kicks, 0);
            assert_eq!(old.device.notifications.rearms, 0);
            assert_eq!(old.device.notifications.reuses, 0);
        }
        old.memory.put(DATA, &[0xcc; 16384]);
        old.memory.put(AVAIL_EVENT, &321u16.to_le_bytes());
        if reset == 0 {
            enable(&mut old.device);
            old.post(0, 0);
        }
        old.backend.ready = 15;
        assert_eq!(old.service(), 0);
        assert_eq!(old.backend.drains, 4);
        assert_eq!(old.memory.word(USED + 2), 0);
        assert_eq!(
            old.memory.word(AVAIL_EVENT),
            if reset == 0 { 1 } else { 321 },
            "only an admitted replacement may rearm its own queue"
        );
        assert_eq!(old.memory.bytes(DATA, 16384), &[0xcc; 16384]);
        assert!(!old.device.irq_pending() || reset == 2); // Fault config IRQ only.
        peer.backend.ready = 1;
        assert_eq!(peer.service(), 1);
        assert!(peer.device.irq_pending());
        assert_eq!(peer.memory.word(AVAIL_EVENT), 1);
        if reset == 0 {
            old.backend.ready = 1;
            assert_eq!(old.service(), 1);
            assert_eq!(old.memory.word(AVAIL_EVENT), 1);
            assert!(old.device.irq_pending());
        }
    }
}
