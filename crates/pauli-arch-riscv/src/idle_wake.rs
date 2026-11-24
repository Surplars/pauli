//! Owner-local selftest observation of the native interrupt state immediately after WFI.
//! Ownership/Lock Ordering: the masked HS owner reads CSRs; no locks or state here.
//! Invariants: each observation enters exactly one bin and never acknowledges an interrupt.
use crate::bits::{SEIP, SSIP, STIP};
use pauli_core::arch::IdleWake;

#[cfg(all(target_arch = "riscv64", feature = "selftest"))]
pub(crate) fn sample() -> IdleWake {
    let (pending, enabled): (usize, usize);
    // SAFETY: native HS dispatcher, SIE masked across WFI and this read. The local
    // enable mask is stable; sample sip last. Interrupt levels can still change.
    unsafe {
        core::arch::asm!(
            "csrr {enabled}, sie", "csrr {pending}, sip",
            enabled = out(reg) enabled, pending = out(reg) pending, options(nostack)
        );
    }
    classify(pending, enabled)
}

fn classify(pending: usize, enabled: usize) -> IdleWake {
    match pending & enabled & (SSIP | STIP | SEIP) {
        STIP => IdleWake::Timer,
        SSIP => IdleWake::Software,
        SEIP => IdleWake::External,
        0 => IdleWake::None,
        _ => IdleWake::Multiple,
    }
}

#[cfg(test)]
#[test]
fn idle_snapshot_classifies_only_locally_enabled_native_sources() {
    use IdleWake::*;
    for (bits, class) in [
        (0, None),
        (STIP, Timer),
        (SSIP, Software),
        (SEIP, External),
        (STIP | SSIP, Multiple),
        (STIP | SEIP, Multiple),
        (SSIP | SEIP, Multiple),
        (STIP | SSIP | SEIP, Multiple),
    ] {
        assert_eq!(classify(bits | crate::bits::VSTIP, usize::MAX), class);
        assert_eq!(classify(usize::MAX, bits), class);
    }
}
