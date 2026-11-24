use super::*;

const READY: &str = "[mb] ready cpus=2 hz=10000000\n";

fn fixture(mut timestamps: impl FnMut(usize) -> (u64, u64, u64)) -> String {
    let mut log = String::from(READY);
    for (v, mode) in MODES.into_iter().enumerate() {
        for i in 0..PER_VISIT {
            let generation = v * PER_VISIT + i + 1;
            let (b, r, e) = timestamps(generation);
            writeln!(
                log,
                "[mb] handoff gen={generation} mode={mode} begin={b} received={r} end={e}"
            )
            .unwrap();
        }
    }
    log.push_str(RECEIPTS);
    log.push_str(DONE);
    log
}

#[test]
fn per_visit_ranks_keep_the_ranked_events_legs_and_skip_warmups() {
    let log = fixture(|g| {
        let b = g as u64 * 2000;
        let i = (g - 1) % PER_VISIT;
        let total = if i < WARMUP { 1500 } else { 35 - i as u64 };
        let outbound = if i.is_multiple_of(2) { total - 2 } else { 2 };
        (b, b + outbound, b + total)
    });
    let mut out = String::new();
    report(
        &mut out,
        parse(log.as_bytes(), READY).unwrap(),
        3,
        10_000_000,
        [0, 1],
    )
    .unwrap();
    assert_eq!(out.matches("phase=warmup").count(), 16);
    assert_eq!(out.matches("phase=scored").count(), 48);
    assert_eq!(out.matches("return_longer=6/12").count(), 4);
    assert_eq!(out.matches(" rank=").count(), 8);
    for (v, mode) in ["busy", "wfi", "wfi", "busy"].into_iter().enumerate() {
        for (rank, gen1, outbound, total) in [("p50", 11, 23, 25), ("max", 5, 29, 31)] {
            let generation = v * 16 + gen1;
            assert!(out.lines().any(|line| line == format!(
                "mailbox-handoff visit={} mode={mode} rank={rank} gen={generation} outbound_ticks={outbound} return_ticks=2 total_ticks={total}", v + 1
            )));
        }
    }
}

#[test]
fn equal_ticks_are_valid_and_ties_break_by_generation() {
    let log = fixture(|_| (u64::MAX, u64::MAX, u64::MAX));
    let mut out = String::new();
    report(
        &mut out,
        parse(log.as_bytes(), READY).unwrap(),
        1,
        10_000_000,
        [0, 0],
    )
    .unwrap();
    assert_eq!(out.matches("return_longer=0/12").count(), 4);
    assert!(out.contains(
        "visit=1 mode=busy rank=p50 gen=10 outbound_ticks=0 return_ticks=0 total_ticks=0"
    ));
    assert!(out.contains(
        "visit=4 mode=busy rank=max gen=64 outbound_ticks=0 return_ticks=0 total_ticks=0"
    ));
}

#[test]
fn rejects_incomplete_corrupt_or_ambiguous_transcripts() {
    let log = fixture(|g| {
        let b = g as u64 * 100;
        (b, b + 20, b + 50)
    });
    let row = log.lines().nth(17).unwrap().to_owned() + "\n";
    for (old, new) in [
        (READY, ""),
        ("cpus=2", "cpus=1"),
        ("hz=10000000", "hz=0"),
        (&row, ""),
        (&row, &row.repeat(2)),
        ("gen=16 ", "gen=17 "),
        ("gen=64 ", "gen=0 "),
        ("gen=17 mode=wfi", "gen=17 mode=busy"),
        ("gen=33 mode=wfi", "gen=33 mode=busy"),
        ("gen=49 mode=busy", "gen=49 mode=wfi"),
        ("begin=1700 ", "begin=1649 "), // overlaps the last event of the preceding visit
        ("received=120 ", "received=99 "), // out of bounds on a warm-up
        ("received=520 ", "received=551 "), // out of bounds on a scored event
        ("received=520 ", ""),          // missing midpoint
        ("end=550\n", "end=550 end=550\n"),
        ("begin=100 ", "begin=+100 "),
        ("received=120 ", "received=18446744073709551616 "),
        ("end=150\n", "end=\n"),
        ("gen=1 ", "gen=18446744073709551616 "),
        ("[mb] handoff", " [mb] handoff"),
        ("ipi=64 ", "ipi=63 "),
        ("wfi_returns=32", "wfi_returns=31"),
        (RECEIPTS, ""),
        (DONE, ""),
        (DONE, "[mb] done"),
        (DONE, "[mb] done\n[mb] done\n"),
        (DONE, "[mb] done\nextra\n"),
    ] {
        assert!(log.contains(old), "invalid test case: {old:?}");
        assert!(
            parse(log.replacen(old, new, 1).as_bytes(), READY).is_err(),
            "accepted {old:?} -> {new:?}"
        );
    }
    let mut non_utf8 = log.into_bytes();
    non_utf8[0] = 0xff;
    assert!(parse(&non_utf8, READY).is_err());
}
