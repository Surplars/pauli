//! Real fixed-storage G-stage maps, including permission and capacity failures.
//! Ownership/Lock Ordering: leaked private fixture tables never execute on a CPU.
//! Invariants: W^X on ordinary leaves; private superpages require unsafe ownership.
use crate::{
    VCpuContext,
    gstage::{GStagePageTable, GStageRoot, GStageTable},
};
use pauli_hal::addr::{MemPerms as P, MmError, PhysAddr as A};
use std::{boxed::Box, vec::Vec};

fn table(count: usize) -> GStagePageTable {
    let root = Box::leak(Box::new(GStageRoot::new()));
    let lower = Box::leak(
        (0..count)
            .map(|_| GStageTable::new())
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    // SAFETY: private aligned fixture storage retained forever, never hardware-active.
    unsafe { GStagePageTable::new_identity(root, lower) }
}
#[test]
fn ordinary_leaves_reject_wx_and_bad_extents() {
    let mut t = table(2);
    let gpa = A::new(0x4000);
    let hpa = A::new(0x9000);
    for perms in [P::WRITE, P::READ | P::WRITE | P::EXECUTE, P::empty()] {
        assert_eq!(
            t.map_page(gpa, hpa, perms),
            Err(MmError::InvalidPermissions)
        );
    }
    assert_eq!(
        t.map_page(A::new(1), hpa, P::READ),
        Err(MmError::InvalidAlignment)
    );
    assert_eq!(
        t.map_page(A::new(1 << 41), hpa, P::READ),
        Err(MmError::InvalidRegion)
    );
    assert_eq!(
        t.map_page(gpa, A::new(1 << 56), P::READ),
        Err(MmError::InvalidRegion)
    );
    t.map_page(gpa, hpa, P::EXECUTE).unwrap();
    assert_eq!(t.translate(A::new(0x4032)).unwrap().raw(), 0x9032);
    assert!(t.is_executable(gpa));
    assert_eq!(t.translate_readable(gpa), None);
    assert_eq!(t.translate(A::new(0x5000)), None);
    assert_eq!(t.map_page(gpa, hpa, P::READ), Err(MmError::AlreadyMapped));
}
#[test]
fn exhausted_tables_remain_fail_closed() {
    let mut t = table(1);
    assert_eq!(
        t.map_page(A::new(0), A::new(0), P::READ),
        Err(MmError::OutOfMemory)
    );
    assert_eq!(t.translate(A::new(0)), None);
    assert_eq!(
        t.map_page(A::new(0), A::new(0), P::READ),
        Err(MmError::OutOfMemory)
    );
}
#[test]
fn private_superpage_and_peer_mappings_are_disjoint() {
    let mut a = table(1);
    let mut b = table(1);
    // SAFETY: scalar model extents represent exclusive disjoint retained RAM;
    // no CPU or host load dereferences these fixture mappings.
    unsafe {
        a.map_private_ram_2m(A::new(1 << 21), A::new(3 << 21))
            .unwrap();
        b.map_private_ram_2m(A::new(1 << 21), A::new(4 << 21))
            .unwrap();
    }
    for offset in [0, 4095, (1 << 21) - 1] {
        assert_eq!(
            a.translate(A::new((1 << 21) + offset)).unwrap().raw(),
            (3 << 21) + offset
        );
        assert_eq!(
            b.translate(A::new((1 << 21) + offset)).unwrap().raw(),
            (4 << 21) + offset
        );
    }
    assert_eq!(a.translate(A::new(2 << 21)), None);
}
#[test]
fn fixed_context_preserves_policy_separation() {
    let mut c = VCpuContext::new();
    assert!(!c.page_faults_delegated());
    c.enable_guest_os();
    assert!(c.page_faults_delegated());
    assert_eq!(c.scounteren, crate::bits::COUNTER_TIME);
    assert_eq!(c.guest_file, 0);
    assert_eq!(c.fp_enabled, 0);
    assert_eq!(core::mem::offset_of!(VCpuContext, x), 0);
    assert_eq!(core::mem::offset_of!(VCpuContext, pc), 32 * 8);
    c.vsstatus = usize::MAX; // Guest CSR fields do not provision F/D or file access.
    assert_eq!((c.fp_enabled, c.guest_file), (0, 0));
}
