//! Host timings for the production PLIC, not guest IRQ latency or an acceptance threshold.
use crate::{interrupt::VirtualInterruptController, plic::VirtualPlic};
use core::hint::black_box;
use std::time::Instant;

const SOURCES: [(u32, u32); 4] = [(1, 2), (2, 1), (10, 4), (11, 7)];
const SINGLE: u32 = 1 << 11;
const ALL: u32 = (1 << 1) | (1 << 2) | (1 << 10) | SINGLE;
const CLAIM: usize = 0x200004;

fn chip(lines: u32, threshold: u32) -> VirtualPlic {
    let mut p = VirtualPlic::new();
    for (id, priority) in SOURCES {
        p.write_word(id as usize * 4, priority).unwrap();
        p.set_level(id, lines & (1 << id) != 0);
    }
    p.write_word(0x2000, ALL).unwrap();
    p.write_word(0x200000, threshold).unwrap();
    p
}

fn sample(label: &str, ops: usize, mut batch: impl FnMut() -> usize, expected: usize) {
    let mut samples = [0u64; 9];
    for round in 0..2 + samples.len() {
        let start = Instant::now();
        let sum = batch();
        let ns = start.elapsed().as_nanos() as u64;
        assert_eq!(sum, expected);
        if round >= 2 {
            samples[round - 2] = ns;
        }
    }
    samples.sort_unstable();
    let per_op = |i: usize| samples[i] as f64 / ops as f64;
    std::println!(
        "PLIC {label}: n={ops} ns/op p25={:.3} median={:.3} p75={:.3}",
        per_op(2),
        per_op(4),
        per_op(6)
    );
}

#[test]
#[ignore = "host cost sample; run explicitly in release mode"]
fn sample_plic_cost() {
    const QUERIES: usize = 1 << 20;
    const PAIRS: usize = 1 << 18;
    // Keep the protocol state opaque for each call. Time reads and setup stay outside the loop.
    for (name, lines, threshold, deliver) in [
        ("empty", 0, 0, false),
        ("single-ready", SINGLE, 0, true),
        ("single-masked", SINGLE, 7, false),
        ("four-early", ALL, 0, true),
        ("four-late", ALL, 5, true),
        ("four-masked", ALL, 7, false),
    ] {
        let p = chip(lines, threshold);
        sample(
            name,
            QUERIES,
            || {
                (0..QUERIES)
                    .map(|_| usize::from(black_box(&p).external_pending_for(0)))
                    .sum()
            },
            usize::from(deliver) * QUERIES,
        );
    }
    for (name, lines) in [
        ("single-claim+complete", SINGLE),
        ("four-claim+complete", ALL),
    ] {
        // A level held through completion re-pends the same source. Claim ignores threshold.
        let mut p = chip(lines, 7);
        sample(
            name,
            PAIRS,
            || {
                (0..PAIRS)
                    .map(|_| {
                        let chip = black_box(&mut p);
                        let id = chip.read_word(CLAIM).unwrap();
                        chip.write_word(CLAIM, id).unwrap();
                        id as usize
                    })
                    .sum()
            },
            11 * PAIRS,
        );
        assert_eq!(
            (p.claims, p.completions),
            (11 * PAIRS as u64, 11 * PAIRS as u64)
        );
    }
}
