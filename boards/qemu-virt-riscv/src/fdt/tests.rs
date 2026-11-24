//! Bind reordered firmware interrupt endpoints to all 64 actual owners.
//! Ownership/Lock Ordering: test-local topology and immutable firmware bytes.
//! Invariants: M contexts never become S targets; duplicates and unknown phandles fail.
use super::*;
use std::vec::Vec;

fn topology() -> Topology {
    let reverse = |i: usize| MAX_CPUS - 1 - i;
    let t = Topology {
        harts: core::array::from_fn(|i| 0x107 + 3 * reverse(i)),
        intc: core::array::from_fn(|i| 702 + reverse(i) as u32),
        count: MAX_CPUS,
        boot: 0,
        fd: true,
        aia: true,
        hz: 10_000_000,
    };
    finish_topology(t, 0x107 + 3 * 63).unwrap()
}
fn list(t: &Topology, m_contexts: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for &phandle in t.intc.iter().rev() {
        if m_contexts {
            out.extend(phandle.to_be_bytes());
            out.extend(11u32.to_be_bytes());
        }
        out.extend(phandle.to_be_bytes());
        out.extend(9u32.to_be_bytes());
    }
    out
}
#[test]
fn high_boot_hart_routes_plic_and_imsic_by_endpoint_not_by_hart_id() {
    let t = topology();
    assert_eq!(t.boot, 63);
    for interleaved in [true, false] {
        let raw = list(&t, interleaved);
        let ctx = supervisor_contexts(&raw, &t).unwrap().unwrap();
        for (i, &route) in ctx.iter().enumerate() {
            let at = MAX_CPUS - 1 - i;
            assert_eq!(t.harts[i], 0x107 + 3 * i);
            assert_eq!(t.intc[i], 702 + i as u32);
            assert_eq!(route, if interleaved { 2 * at + 1 } else { at });
        }
        assert_eq!(
            supervisor_contexts(&raw[..raw.len() - 8], &t).unwrap(),
            None
        );
        let mut duplicate = raw.clone();
        duplicate.extend_from_slice(&raw[raw.len() - 8..]);
        assert!(supervisor_contexts(&duplicate, &t).is_err());
        let mut unknown = raw;
        let last = unknown.len() - 8;
        unknown[last..last + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(supervisor_contexts(&unknown, &t).is_err());
    }
}
