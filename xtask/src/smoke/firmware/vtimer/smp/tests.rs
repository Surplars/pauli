//! Replay native placement and remote epoch witnesses without an emulator.
//! Each echoed byte belongs to its scored source; bounds or statuses alone
//! cannot substitute for complete timer receipts before, inside and after stop.
use super::super::replay::Wire;
use super::*;

fn row(generation: u64, due: u64, trap: u64) -> Row {
    Row {
        generation,
        due,
        armed: due - 1,
        trap,
        pc: "post",
    }
}

#[test]
fn lifecycle_requires_completed_receipts_not_just_intersecting_intervals() {
    let e = Edges {
        old_live: 100,
        joined: 130,
        pre_start: 170,
        fresh_live: 200,
    };
    let before = row(1, 50, 99);
    let during = row(2, 131, 169);
    let after = row(3, 201, 205);
    assert_eq!(e.counts(&[before, during, after]).unwrap(), [1, 1, 1]);
    for bad in [
        [row(1, 50, 100), during, after],
        [before, row(2, 130, 150), after],
        [before, row(2, 150, 170), after],
        [before, during, row(3, 200, 205)],
    ] {
        assert!(e.counts(&bad).is_err());
    }
    assert!(
        Edges {
            joined: e.pre_start,
            ..e
        }
        .counts(&[before, during, after])
        .is_err()
    );
}

fn worker(name: &'static str, epoch: u32, owner: u8) -> Worker {
    Worker { name, epoch, owner }
}
fn command(w: &mut Wire, worker: &Worker, key: u8, response: &str) {
    let epoch = worker.epoch;
    w.command(
        &format!("vm attach {}", worker.name),
        &format!("\n\x1eA{epoch:08x};"),
    );
    w.step(key, &format!("\x1eG{epoch:08x};{response}"));
    w.step(0x1d, "\x1eD\x1eHhv> ");
}
fn stamp(w: &mut Wire, p: &Worker, t: u64) {
    command(w, p, b'n', &format!("[fair] now={t:016x}\n"));
}

#[test]
fn stopped_worker_must_retire_and_return_with_a_new_epoch_on_the_same_hart() {
    const BASE: u64 = HZ;
    for (owner, advertised, retired, fresh_epoch, retained, valid) in [
        (0, 0, true, 5, 0, true),
        (1, 1, true, 5, 0, true),
        (0, 1, true, 5, 0, false),
        (1, 1, false, 5, 0, false),
        (1, 1, true, 4, 0, false),
        (1, 1, true, 5, 1, false),
    ] {
        let mut w = Wire::default();
        let p = Probe {
            id: 1,
            name: "control",
            epoch: 1,
            owner: 1 - owner,
            next_gen: 51,
            cursor: 0,
            last_trap: 0,
        };
        let mut peers = [
            worker("service", 2, owner),
            worker("busy2", 3, owner),
            worker("busy3", 4, owner),
        ];
        stamp(&mut w, &peers[2], BASE + 5 * STEP);
        w.command("vm stop busy3", "\nVM 4 epoch=4 Stopped\nhv> ");
        w.command("vm status busy3", &format!("\nVM id=4 epoch=4 name=busy3 state=Stopped vcpus=1 memory=32KiB\n  vcpu=0 owner={owner} state=Stopped retired={retired} run=Stopped\nhv> "));
        stamp(&mut w, &peers[0], BASE + 7 * STEP);
        stamp(&mut w, &peers[0], BASE + 10 * STEP);
        w.command("vm start busy3", "\nhv> ");
        let fresh = worker("busy3", fresh_epoch, owner);
        w.command("vm status busy3", &format!("\nVM id=4 epoch={fresh_epoch} name=busy3 state=Ready vcpus=1 memory=32KiB\n  vcpu=0 owner={advertised} state=Started retired=false run=Waiting\nhv> \x1eG{fresh_epoch:08x};[fair] ready\n\x1eH"));
        command(
            &mut w,
            &fresh,
            b'x',
            &format!(
                "[fair] batches={retained:016x} first={retained:016x} last={retained:016x} max_gap={retained:016x}\n"
            ),
        );
        command(&mut w, &fresh, b'm', "[fair] measuring\n");
        stamp(&mut w, &fresh, BASE + 16 * STEP);
        let got = change_epoch(&mut w.controller(), &p, &mut peers, BASE);
        assert_eq!(got.is_ok(), valid);
        if let Ok(e) = got {
            assert_eq!(peers[2].epoch, fresh_epoch);
            assert_eq!(
                e.counts(&[
                    row(1, BASE + 4 * STEP, BASE + 4 * STEP + 1),
                    row(2, BASE + 8 * STEP, BASE + 8 * STEP + 1),
                    row(3, BASE + 18 * STEP, BASE + 18 * STEP + 1)
                ])
                .unwrap(),
                [1, 1, 1]
            );
        }
    }
}
