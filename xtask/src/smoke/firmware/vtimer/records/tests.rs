use super::*;
use std::fmt::Write as _;

fn wire(
    first_gen: u64,
    began: u64,
    mut event: impl FnMut(usize, u64) -> (u64, u64, &'static str),
) -> String {
    let mut began = began;
    let mut log = String::new();
    for i in 0..EVENTS {
        let due = began + LEAD;
        let (armed, trap, pc) = event(i, due);
        let generation = first_gen.wrapping_add(i as u64); // Also generate the invalid wrap case.
        writeln!(
            log,
            "[vt] gen={generation:016x} due={due:016x} armed={armed:016x} trap={trap:016x} pc={pc}"
        )
        .unwrap();
        began = trap;
    }
    log.push_str("[vt] done\n");
    log
}

fn basic(first_gen: u64, began: u64) -> String {
    wire(first_gen, began, |_, due| (due - LEAD, due, "pre"))
}

#[test]
fn exact_visit_rejects_foreign_missing_duplicate_and_malformed_rows() {
    let log = basic(10, 0);
    assert!(parse(log.as_bytes(), 10, 0).is_ok());
    let (first, rest) = log.split_once('\n').unwrap();
    for bad in [
        format!("hv> {log}"),
        format!("{log}junk"),
        format!("{log}\n"),
        format!("{log}{log}"),
        format!("{first}\n{log}"),
        rest.to_owned(),
        log.replace("[vt] done\n", ""),
        log.replace("\n[vt] done", "[vt] done"),
        log.replace("[vt] done\n", "[vt] done"),
        log.replace("\n", "\r\n"),
        log.replacen("000000000000000a", "000000000000000A", 1),
        log.replacen("000000000000000a", "+00000000000000a", 1),
        log.replacen("000000000000000a", "a", 1),
        log.replacen(" due=", "  due=", 1),
        log.replacen(" due=", " armed=", 1),
        log.replacen("pc=pre", "pc=wfi", 1),
        log.replacen("pc=pre", "pc=pre junk=1", 1),
        log.replacen("gen=000000000000000b", "gen=000000000000000a", 1),
        log.replacen("gen=000000000000000b", "gen=000000000000000c", 1),
    ] {
        assert!(parse(bad.as_bytes(), 10, 0).is_err(), "{bad}");
    }
    let mut invalid = log.into_bytes();
    invalid[0] = 0xff;
    assert!(parse(&invalid, 10, 0).is_err());
}

#[test]
fn visits_join_at_equal_ticks_but_not_with_replayed_samples_or_overflow() -> Result<()> {
    let last = parse(basic(1, 0).as_bytes(), 1, 0)?.last_trap();
    let next = basic(25, last);
    assert_eq!(
        parse(next.as_bytes(), 25, last)?.last_trap(),
        last + EVENTS as u64 * LEAD
    );
    assert!(parse(next.as_bytes(), 1, last).is_err());
    assert!(parse(next.as_bytes(), 25, last + 1).is_err());
    let max_start = u64::MAX - (EVENTS as u64 - 1);
    let max_time = u64::MAX - EVENTS as u64 * LEAD;
    assert_eq!(
        parse(basic(max_start, max_time).as_bytes(), max_start, max_time)?.last_trap(),
        u64::MAX
    );
    assert!(parse(basic(max_start + 1, 0).as_bytes(), max_start + 1, 0).is_err());
    Ok(())
}

#[test]
fn rejects_each_broken_causal_edge_but_accepts_late_arming_and_equal_timestamps() {
    let log = basic(1, 5);
    let replace = |key, from: u64, to: u64| {
        log.replacen(
            &format!("{key}={from:016x}"),
            &format!("{key}={to:016x}"),
            1,
        )
    };
    for bad in [
        replace("due", LEAD + 5, LEAD - 1),
        replace("due", LEAD * 2 + 5, LEAD * 2 + 4), // Second start overlaps first trap.
        replace("armed", 5, 4),
        replace("trap", LEAD + 5, LEAD + 4),
        replace("armed", 5, LEAD + 6), // Trap is due but predates arm.
    ] {
        assert!(parse(bad.as_bytes(), 1, 5).is_err());
    }
    let log = wire(1, 5, |i, due| {
        let armed = due + (i % 2) as u64;
        (armed, armed, "post")
    });
    let visit = parse(log.as_bytes(), 1, 5).unwrap();
    let mut out = Vec::new();
    visit.write_report(&mut out, 7, 2, "wfi").unwrap();
    let report = String::from_utf8(out).unwrap();
    assert!(report.contains("matched_gen=20 due_at_arm=20 pc_pre=0 pc_post=20"));
    assert!(report.contains("armed_before_due_pc_post=0/20"));
    assert!(!report.contains("armed_before_due_pc_post p50:"));
}

#[test]
fn ranks_exclude_warmup_preserve_tied_events_and_report_raw_evidence() -> Result<()> {
    let log = wire(1, 100, |i, due| {
        if i < WARMUP {
            return (due, due + 5_000, "post"); // Excluded even from the maximum.
        }
        let rank = (i - WARMUP + 1) as u64;
        match rank {
            10 => (due + 7, due + 10, "pre"),
            11 => (due - 5, due + 10, "post"),
            19 => (due + 13, due + 19, "pre"),
            20 => (due, due + 19, "post"),
            _ => (due - 1, due + rank, "post"),
        }
    });
    let visit = parse(log.as_bytes(), 1, 0)?;
    let mut out = Vec::new();
    visit.write_report(&mut out, 7, 2, "busy")?;
    let report = std::str::from_utf8(&out)?;
    for needle in [
        "epoch=7 round=2 mode=busy timebase_hz=10000000 lead_ticks=10000 warmup=4 scored=20",
        &log,
        "matched_gen=20 due_at_arm=3 pc_pre=2 pc_post=18",
        "all p50: rank=10/20 gen=000000000000000e pc=pre late_at_arm_ticks=7 after_both_ticks=3 total_ticks=10",
        "all p95: rank=19/20 gen=0000000000000017 pc=pre late_at_arm_ticks=13 after_both_ticks=6 total_ticks=19",
        "all max: rank=20/20 gen=0000000000000018 pc=post late_at_arm_ticks=0 after_both_ticks=19 total_ticks=19",
        "armed_before_due_pc_post=17/20",
        "armed_before_due_pc_post p50: rank=9/17",
        "armed_before_due_pc_post p95: rank=17/17",
    ] {
        assert!(report.contains(needle), "{needle}\n{report}");
    }
    Ok(())
}
