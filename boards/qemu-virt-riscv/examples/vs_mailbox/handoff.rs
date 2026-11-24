//! Guest-only IPI handoff: timestamp a real SSIP, then either poll or wait.
//! No locks: the sender owns requests and results; the peer owns receipts and
//! WFI accounting. One outstanding generation keeps the midpoint paired:
//! request -> SSIP -> receipt -> next request. Console output follows hart stop.

use super::{
    Acquire, AtomicUsize, BAD, IPI, ONE_HART_MASK, OP_SECONDS, PEER, RECEIPT, REQUEST, Relaxed,
    Release, STOP, Uart, Write, asm, bits, ok, stop_peer, take_ssip, ticks, until,
};
use core::sync::atomic::AtomicU64;

const VISIT: usize = 4 + 12; // Warm-up and scored generations for each visit.
const TOTAL: usize = 4 * VISIT;
static RECEIVED_AT: AtomicU64 = AtomicU64::new(0);
static WFI_RETURNS: AtomicUsize = AtomicUsize::new(0);

fn use_wfi(generation: usize) -> bool {
    matches!((generation - 1) / VISIT, 1 | 2) // busy, WFI, WFI, busy
}

/// TIME reads are device inputs on RISC-V. Order the generation/receipt load
/// before the midpoint/endpoint, including on weakly ordered hardware.
fn ticks_after_load() -> u64 {
    let value;
    // SAFETY: VS TIME is enabled and a memory-to-CSR fence needs no address.
    unsafe { asm!("fence r, i", "rdtime {}", out(reg) value, options(nostack)) };
    value
}

pub(super) fn receive() -> bool {
    let mut seen = 0;
    let mut returns = 0;
    while !STOP.load(Acquire) {
        if !take_ssip() {
            continue;
        }
        seen += 1;
        if REQUEST.load(Acquire) != seen {
            return false;
        }
        // One request is outstanding. The release receipt publishes its exact
        // midpoint; the sender consumes both before issuing the next IPI.
        RECEIVED_AT.store(ticks_after_load(), Relaxed);
        RECEIPT.store(seen, Release);
        if use_wfi(seen) {
            // Enable only local SSIE, not global SIE. An already pending SSIP
            // wakes WFI too. A spurious return still requires a real SSIP above.
            // SAFETY: these are the worker's own VS interrupt state and wait.
            unsafe {
                asm!("csrsi sie, {ssie}", "wfi", "csrci sie, {ssie}",
                    ssie = const bits::SSIP, options(nostack));
            }
            returns += 1;
        }
    }
    WFI_RETURNS.store(returns, Release);
    true
}

pub(super) fn run(hz: u64) {
    let mut samples = [(0, 0, 0); TOTAL];
    for (index, sample) in samples.iter_mut().enumerate() {
        let generation = index + 1;
        REQUEST.store(generation, Release);
        let begin = ticks();
        ok(IPI, 0, [ONE_HART_MASK, PEER, 0, 0]);
        until(begin + OP_SECONDS * hz, || {
            let receipt = RECEIPT.load(Acquire);
            assert_ne!(receipt, BAD, "worker rejected IPI sequence");
            receipt == generation
        });
        let end = ticks_after_load();
        let received = RECEIVED_AT.load(Relaxed);
        assert!(
            begin <= received && received <= end,
            "invalid handoff midpoint"
        );
        assert!(end - begin < OP_SECONDS * hz, "IPI handoff timed out");
        *sample = (begin, received, end);
    }
    // The final busy visit lets the peer observe STOP without a synthetic IPI.
    let receipts = stop_peer(hz);
    let returned = WFI_RETURNS.load(Acquire);
    assert_eq!(receipts, TOTAL);
    assert_eq!(returned, 2 * VISIT);
    for (index, (begin, received, end)) in samples.iter().enumerate() {
        let generation = index + 1;
        let mode = if use_wfi(generation) { "wfi" } else { "busy" };
        writeln!(
            Uart,
            "[mb] handoff gen={generation} mode={mode} begin={begin} received={received} end={end}"
        )
        .unwrap();
    }
    writeln!(Uart, "[mb] receipts ipi={receipts} wfi_returns={returned}").unwrap();
}
