//! Exercise common-clock evidence and the real monitor protocol without QEMU.
//! Each input releases only its own fragmented UART reply; epochs never share a cursor.
use super::super::replay::{self, CONTROL, STOPPED, Wire, samples};
use super::*;

fn metrics(n: u64, first: u64, last: u64, gap: u64) -> String {
    format!("[fair] batches={n:016x} first={first:016x} last={last:016x} max_gap={gap:016x}\n")
}

#[test]
fn worker_wire_is_exact_and_a_consumed_record_is_all_zero() {
    let zero = metrics(0, 0, 0, 0);
    assert_eq!(Window::parse(zero.as_bytes()).unwrap(), Window::default());
    for bad in [
        zero.trim_end().to_owned(),
        zero.replace("batches=", "batch="),
        zero.replacen("0000000000000000", "1", 1),
        zero.replacen("0000000000000000", "FFFFFFFFFFFFFFFF", 1),
        zero.replace("first=", "last="),
        format!("{zero}{zero}"),
    ] {
        assert!(Window::parse(bad.as_bytes()).is_err());
    }
}

#[test]
fn a_positive_counter_alone_does_not_prove_progress_inside_the_probe_span() {
    let w = Window {
        batches: 10,
        first: 100,
        last: 200,
        max_gap: 10,
    };
    assert_eq!(w.progress_inside((120, 140)).unwrap(), 1);
    assert_eq!(w.progress_inside((120, 141)).unwrap(), 2);
    assert_eq!(w.progress_inside((100, 200)).unwrap(), 9);
    for span in [(99, 199), (110, 201), (100, 100), (140, 120), (115, 125)] {
        assert!(w.progress_inside(span).is_err(), "{span:?}");
    }
    for bad in [
        Window { max_gap: 0, ..w },
        Window { max_gap: 101, ..w },
        Window { batches: 9, ..w },
    ] {
        assert!(bad.progress_inside((120, 140)).is_err());
    }
    let boundary = Window {
        batches: u64::MAX,
        first: 0,
        last: u64::MAX,
        max_gap: 1,
    };
    assert_eq!(
        boundary.progress_inside((0, u64::MAX)).unwrap(),
        u64::MAX - 1
    );
}

fn peers() -> [Worker; 2] {
    [
        Worker {
            name: "service",
            epoch: 2,
            owner: 0,
        },
        Worker {
            name: "peer2",
            epoch: 3,
            owner: 0,
        },
    ]
}

fn exchange(w: &mut Wire, worker: &Worker, key: u8, reply: &str, source_epoch: u32) {
    w.command(
        &format!("vm attach {}", worker.name),
        &format!("\n\x1eA{:08x};", worker.epoch),
    );
    w.step(key, &format!("\x1eG{source_epoch:08x};{reply}"));
    w.step(0x1d, "\x1eD\x1eHhv> ");
}

fn busy(w: &mut Wire, peers: &[Worker; 2]) {
    for peer in peers {
        exchange(w, peer, b'm', "[fair] measuring\n", peer.epoch);
    }
}

fn ready(w: &mut Wire, p: &Worker, id: u32, after: &str) {
    w.command(&format!("vm status {}", p.name), &format!(
        "\nVM id={id} epoch={} name={} state=Ready vcpus=1 memory=32KiB\n  vcpu=0 owner=0 state=Started retired=false run=Waiting\nhv> {after}",
        p.epoch, p.name));
}

#[test]
fn idle_control_checks_both_parked_vcpus_and_rejects_residual_work() -> Result<()> {
    let peers = peers();
    for clean in [true, false] {
        let mut w = Wire::boot(CONTROL, replay::READY);
        ready(&mut w, &peers[0], 2, "");
        ready(&mut w, &peers[1], 3, "");
        w.capture(CONTROL, &samples(1), "");
        exchange(&mut w, &peers[0], b'x', &metrics(0, 0, 0, 0), 2);
        let n = u64::from(!clean);
        exchange(&mut w, &peers[1], b'x', &metrics(n, 0, n, n), 3);
        let mut c = w.controller();
        let mut p = Probe::start(&mut c)?;
        assert_eq!(round(&mut c, &mut p, &peers, false).is_ok(), clean);
    }
    Ok(())
}

#[test]
fn both_workers_must_report_for_the_same_probe_window_on_their_own_console() -> Result<()> {
    let peers = peers();
    let span = metrics(100, 0, 500_000, 5_000);
    for (epoch, second, valid) in [
        (3, span.clone(), true),
        (2, span.clone(), false),
        (3, metrics(100, 90_001, 590_001, 5_000), false),
        (3, metrics(100, 0, 500_000, 400_000), false),
    ] {
        let mut w = Wire::boot(CONTROL, replay::READY);
        busy(&mut w, &peers);
        w.capture(CONTROL, &samples(1), "");
        exchange(&mut w, &peers[0], b'x', &span, 2);
        exchange(&mut w, &peers[1], b'x', &second, epoch);
        let mut c = w.controller();
        let mut p = Probe::start(&mut c)?;
        let result = round(&mut c, &mut p, &peers, true);
        assert_eq!(result.is_ok(), valid);
        if let Ok((visit, windows)) = result {
            assert_eq!(visit.scored_window(), (90_000, 480_001));
            assert_eq!(
                windows.iter().map(|(_, n)| *n).collect::<Vec<_>>(),
                [78, 78]
            );
        }
    }
    Ok(())
}

#[test]
fn stop_joins_the_busy_clone_and_fresh_epoch_cannot_claim_its_predecessors_window() -> Result<()> {
    let peers = peers();
    let fresh = Worker {
        name: "peer2",
        epoch: 4,
        owner: 0,
    };
    for cold_clear in [true, false] {
        let mut w = Wire::boot(CONTROL, replay::READY);
        busy(&mut w, &peers);
        w.command("vm stop peer2", "\nVM 3 epoch=3 Stopped\nhv> ");
        w.capture(CONTROL, &samples(1), "");
        w.command("vm start peer2", "\nhv> ");
        ready(&mut w, &fresh, 3, "\x1eG00000004;[fair] ready\n\x1eH");
        let n = u64::from(!cold_clear);
        exchange(&mut w, &fresh, b'x', &metrics(n, 0, n, n), 4);
        w.capture(CONTROL, &samples(25), "");
        exchange(
            &mut w,
            &peers[0],
            b'x',
            &metrics(1000, 0, 1_000_000, 1000),
            2,
        );
        w.command("vm stop peer2", "\nVM 3 epoch=4 Stopped\nhv> ");
        w.command("vm destroy peer2", "\nVM 3 destroyed\nhv> ");
        ready(&mut w, &peers[0], 2, "");
        w.shutdown("", STOPPED);
        let mut c = w.controller();
        let mut p = Probe::start(&mut c)?;
        let result = restart(&mut c, &mut p, &peers);
        if cold_clear {
            result?;
        } else {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("retained an open measurement")
            );
        }
    }
    Ok(())
}
