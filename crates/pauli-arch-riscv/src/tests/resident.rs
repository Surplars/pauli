//! Production algorithm acceptance, using private host fixtures.
//! Ownership/Lock Ordering: each test owns its state; no actual CSR access.
//! Invariants: retain the original protocol assertions and rejection cases.
use crate::resident::state;
use state::{FULL, LOAD, RESUME, State, Statistics};

#[test]
fn acceptance() {
    assert_eq!((FULL, LOAD, RESUME), (0, 1, 2));
    let mut total = Statistics::default();
    // Each iteration represents a new slice/epoch. No warm state survives
    // stop, fault, HSM reset, VMID reuse or a switch to the peer's context.
    for short_exits in [0, 1, 9, 256] {
        let mut owner = State::default();
        assert!(!owner.put()); // Failed lease/timer before first entry.
        assert_eq!(owner.enter(), LOAD);
        for _ in 0..short_exits {
            assert_eq!(owner.enter(), RESUME);
        }
        assert!(owner.put());
        assert!(!owner.put()); // Explicit control cleanup + lexical cleanup.
        assert_eq!(owner.enter(), LOAD);
        assert_eq!(owner.enter(), RESUME);
        assert!(owner.put());
        assert!(!owner.loaded);
        assert_eq!(owner.statistics.loads, owner.statistics.puts);
        assert_eq!(owner.statistics.resumes, short_exits + 1);
        total.merge(owner.statistics);
    }
    assert_eq!((total.loads, total.puts, total.resumes), (8, 8, 270));
    std::println!(
        "resident ownership PASS: load/resume/put, early errors, control reload and epoch reset"
    );
}

#[test]
fn resident_fp_ownership_loads_once_per_slice_and_puts_idempotently() {
    let mut fp_guest = State::default();
    assert!(!fp_guest.put_fp());
    assert!(fp_guest.load_fp(true));
    assert_eq!(fp_guest.enter(), LOAD);
    for _ in 0..16 {
        // Subsequent quick exits keep the hardware F/D bank live.
        assert!(!fp_guest.load_fp(true));
        assert_eq!(fp_guest.enter(), RESUME);
    }
    assert!(fp_guest.fp_loaded);
    assert!(fp_guest.put_fp());
    assert!(fp_guest.put());
    assert!(!fp_guest.fp_loaded);
    assert!(!fp_guest.put_fp());
    assert!(!fp_guest.put());
    assert_eq!(
        (
            fp_guest.statistics.fp_loads,
            fp_guest.statistics.fp_puts,
            fp_guest.statistics.loads,
            fp_guest.statistics.puts,
            fp_guest.statistics.resumes,
        ),
        (1, 1, 1, 1, 16)
    );

    // Integer firmware vCPUs never acquire or flush F/D state.
    let mut integer_guest = State::default();
    assert!(!integer_guest.load_fp(false));
    assert_eq!(integer_guest.enter(), LOAD);
    assert!(!integer_guest.fp_loaded);
    assert!(!integer_guest.put_fp());
    assert!(integer_guest.put());
    assert_eq!(
        (integer_guest.statistics.fp_loads, integer_guest.statistics.fp_puts),
        (0, 0)
    );
}
