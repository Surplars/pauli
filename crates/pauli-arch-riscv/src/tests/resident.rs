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
