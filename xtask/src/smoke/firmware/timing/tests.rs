use super::*;
use crate::smoke::firmware::tests::Replay;
use pauli_devices::console::Encoder;
use std::time::Duration;

const BOOT: &str = "[    0] [rtos] timing-v1\r\n[    0] [rtos] scheduler ready hz=10000000\n";
const H1: &str = "[rtos] latency samples=64 lt2ms=28 lt10ms=2 lt20ms=1 ge20ms=1\n";
const H2: &str = "[rtos] latency samples=96 lt2ms=31 lt10ms=1 lt20ms=0 ge20ms=0\n";
const R1: &str = "[rtos] samples=64 max_late_ticks=500000 missed=7 window_max_ticks=400000\n";
const R2: &str = "[rtos] samples=96 max_late_ticks=500000 missed=7 window_max_ticks=20000\n";

#[test]
fn require_capability_before_readiness() {
    assert!(require_probe(BOOT.as_bytes()).is_ok());
    for bad in [
        BOOT.replace("v1", "v10"),
        BOOT.lines().rev().collect::<Vec<_>>().join("\n"),
    ] {
        assert!(require_probe(bad.as_bytes()).is_err());
    }
}

// These tests record multiple consoles through the same encoder used by HS.
// A marker delivered in an unrelated epoch cannot fill a missing histogram.
fn replay(parts: &[(u32, &str)]) -> Result<([u64; 2], [u64; 4])> {
    let mut bytes = Vec::new();
    let mut encoder = Encoder::new();
    for &(epoch, record) in parts {
        for b in record.bytes() {
            encoder.write(Source::Guest(epoch), b, |b| bytes.push(b));
        }
    }
    let mut c = Controller::new(
        Replay(bytes.chunks(17).map(<[u8]>::to_vec).collect()),
        Duration::from_secs(2),
    );
    let (_, peaks, bins) = pair(
        &mut c,
        1,
        &mut 0,
        Boundary {
            samples: 32,
            missed: 5,
        },
        10_000_000,
    )?;
    Ok((peaks, bins))
}

#[test]
fn pair_needs_two_post_measurement_histograms_from_current_epoch() {
    assert_eq!(
        replay(&[(1, R1), (1, R2), (1, H1), (1, H2)]).unwrap(),
        ([400000, 20000], [59, 3, 1, 1])
    );
    for bad in [
        [(1, R1), (1, H1), (1, R2), (1, H2)], // Additional UART during timing.
        [(1, R1), (1, R2), (1, H2), (1, H1)],
        [(1, R1), (1, R2), (2, H1), (1, H2)],
        [(1, R1), (1, R2), (1, H1), (1, H1)],
    ] {
        assert!(replay(&bad).is_err());
    }
}

#[test]
fn bins_match_window_max_duration_and_skipped_period_count() {
    let anchor = Boundary {
        samples: 32,
        missed: 5,
    };
    let data = "64 lt2ms=28 lt10ms=2 lt20ms=1 ge20ms=1";
    let check = |row: &str, peak, missed, hz| {
        histogram(
            row,
            anchor,
            &Report {
                end: Boundary {
                    samples: 64,
                    missed,
                },
                peak,
            },
            hz,
        )
        .is_ok()
    };
    assert!(check(data, 400000, 7, 10_000_000));
    for (from, to) in [
        ("28", "29"),
        ("28", "+28"),
        ("28", "18446744073709551615"),
        ("lt10ms", "lt2ms"),
        ("ge20ms=1", "ge20ms=1 extra=0"),
    ] {
        assert!(!check(&data.replace(from, to), 400000, 7, 10_000_000));
    }
    assert!(!check(data, 199999, 7, 10_000_000));
    assert!(!check(data, 400000, 6, 10_000_000));
    assert!(!check(data, 399999, 7, 10_000_000));
    // 1001 Hz: a logical period is 20 ticks, but the real 20 ms bin starts at 21.
    let odd = "64 lt2ms=31 lt10ms=0 lt20ms=1 ge20ms=0";
    assert!(check(odd, 20, 6, 1001));
    assert!(!check(odd, 21, 6, 1001));
}
