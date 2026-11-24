use super::super::{parse, tests::split_fixture};
use super::*;

const READY: &str = "[mb] ready cpus=2 hz=10000000\n";

#[test]
fn joins_exact_generations_and_keeps_both_parts_of_the_same_event() {
    let (guest, host) = split_fixture(0);
    let windows = parse(guest.as_bytes(), READY, true).unwrap();
    let events = join(host.as_bytes(), 7, 0, &windows).unwrap();
    for rows in events {
        for (n, row) in rows.iter().enumerate() {
            assert_eq!(row.generation, WARMUP + n + 1);
            assert_eq!(
                row.path,
                if row.generation.is_multiple_of(2) {
                    "direct"
                } else {
                    "deferred"
                }
            );
            assert_eq!(
                (row.completed - row.begin, row.end - row.completed),
                (15, 25)
            );
        }
    }
    // Equal clock ticks are legal at either end of the interval.
    for completion in [1300, 1340] {
        let equal = host.replace(
            "vma gen=5 ticks=1315",
            &format!("vma gen=5 ticks={completion}"),
        );
        join(equal.as_bytes(), 7, 0, &windows).unwrap();
    }
}

#[test]
fn rejects_any_missing_ambiguous_stale_or_unbracketed_target_record() {
    let (guest, host) = split_fixture(0);
    let windows = parse(guest.as_bytes(), READY, true).unwrap();
    let warm = host.lines().find(|s| s.contains("vma gen=1 ")).unwrap();
    for (old, new) in [
        ("epoch=7", "epoch=8"),
        ("target=1", "target=0"),
        ("owner=0", "owner=1"),
        ("requester=0", "requester=1"),
        ("op=vma", "op=ipi"),
        ("gen=28 ", "gen=29 "),
        ("gen=5 ", "gen=4 "),
        (warm, ""),
        ("vma gen=1 ticks=215", "vma gen=1 ticks=3315"),
        ("vma gen=5 ticks=1315", "vma gen=5 ticks=1299"),
        ("vma gen=5 ticks=1315", "vma gen=5 ticks=1341"),
        // Reject unproven paths even on warm-ups: all 56 tickets must be accounted for.
        ("ticks=215 path=deferred", "ticks=215"),
        ("ticks=215 path=deferred", "ticks=215 path=unobserved"),
        ("ticks=1315 path=deferred", "ticks=1315 path=unknown"),
        (
            "ticks=1315 path=deferred",
            "ticks=1315 path=direct path=deferred",
        ),
        ("hv> ", "unexpected\nhv> "),
        ("hv> ", ""),
    ] {
        assert!(host.contains(old), "invalid test input {old}");
        assert!(
            join(host.replacen(old, new, 1).as_bytes(), 7, 0, &windows).is_err(),
            "accepted {old} -> {new}"
        );
    }
}

#[test]
fn path_subsets_have_no_ranks_when_empty_and_keep_each_ranked_ticket() {
    let mut out = String::new();
    ranked(&mut out, "vma", Some("direct"), &[]).unwrap();
    assert_eq!(out, "mailbox-split group op=vma path=direct n=0\n");
    out.clear();
    let rows = [
        Event {
            generation: 6,
            begin: 10,
            completed: 18,
            end: 19,
            path: "deferred",
        },
        Event {
            generation: 5,
            begin: 2,
            completed: 3,
            end: 15,
            path: "deferred",
        },
    ];
    ranked(&mut out, "vma", Some("deferred"), &rows).unwrap();
    assert_eq!(
        out,
        concat!(
            "mailbox-split group op=vma path=deferred n=2\n",
            "mailbox-split group op=vma rank=p50 gen=6 outward_ticks=8 return_ticks=1 total_ticks=9 path=deferred\n",
            "mailbox-split group op=vma rank=max gen=5 outward_ticks=1 return_ticks=12 total_ticks=13 path=deferred\n",
        )
    );
}

#[test]
fn guest_split_requires_ordered_nonoverlapping_windows_and_checked_end() {
    let (guest, _) = split_fixture(0);
    let mut swapped: Vec<_> = guest.lines().collect();
    swapped.swap(1, 2);
    assert!(parse((swapped.join("\n") + "\n").as_bytes(), READY, true).is_err());
    for (old, new) in [
        (" start=1300", ""),
        (" start=1400", " start=1319"),
        (" start=1300", " start=18446744073709551615"),
        (" start=1300", " start=+1300"),
        ("ticks=40", "ticks=4x"),
    ] {
        assert!(
            parse(guest.replacen(old, new, 1).as_bytes(), READY, true).is_err(),
            "{old} -> {new}"
        );
    }
    assert!(parse(guest.as_bytes(), READY, false).is_err());
}
