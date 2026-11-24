//! Replay fragmented, epoch-tagged arm echoes and coalesced asynchronous reports.
//! One fixture owns each serial exchange; a missing peer, compare or guard is an error.
use super::super::replay::Wire;
use super::*;
use std::fmt::Write;

fn rows(base: u64, skew: u64) -> String {
    let mut s = String::new();
    for i in 0..COUNT as u64 {
        let due = base + i * STEP;
        writeln!(
            s,
            "[vt] gen={:016x} due={due:016x} armed={:016x} trap={:016x} pc=post",
            i + 1,
            due - 5,
            due + skew
        )
        .unwrap();
    }
    s.push_str(DONE);
    s
}

#[test]
fn both_entire_grids_must_be_early_armed_including_the_uart_guard() {
    let base = HZ;
    let good = rows(base, 10);
    assert_eq!(decode(good.as_bytes(), 1, base, 0).unwrap().len(), COUNT);
    let guard = base + GUARD as u64 * STEP;
    for bad in [
        rows(base + 1, 10),
        good.replace(
            &format!("armed={:016x}", guard - 5),
            &format!("armed={guard:016x}"),
        ),
        good.replace(
            &format!("trap={:016x}", base + 10),
            &format!("trap={:016x}", base + STEP - 4),
        ),
        good.replacen("gen=0000000000000002", "gen=0000000000000001", 1),
        good.split_once('\n').unwrap().1.to_owned(),
        format!("{good}{DONE}"),
    ] {
        assert!(decode(bad.as_bytes(), 1, base, 0).is_err());
    }
    assert!(decode(good.as_bytes(), 1, base, base).is_err());
}

fn attach(w: &mut Wire, name: &str, epoch: u32) {
    w.command(&format!("vm attach {name}"), &format!("\x1eA{epoch:08x};"));
}
fn tagged(epoch: u32, data: &str) -> String {
    format!("\x1eG{epoch:08x};{data}")
}
fn detach(w: &mut Wire, async_report: &str) {
    w.step(0x1d, &format!("\x1eD\x1eHhv> {async_report}\x1eH"));
}
fn worker_command(w: &mut Wire, worker: &Worker, key: u8, reply: &str) {
    attach(w, worker.name, worker.epoch);
    w.step(key, &tagged(worker.epoch, reply));
    detach(w, "");
}
fn queue(w: &mut Wire, name: &str, epoch: u32, base: u64, async_report: &str) {
    attach(w, name, epoch);
    w.step(b'a', &tagged(epoch, "[vt] arm\n"));
    for (i, b) in format!("{base:016x}").bytes().enumerate() {
        let suffix = if i == 15 { "\n[vt] queued\n" } else { "" };
        w.step(b, &tagged(epoch, &format!("{}{suffix}", char::from(b))));
    }
    detach(w, async_report);
}

#[test]
fn cloned_timer_requires_its_own_native_shutdown_and_retirement() {
    for (id, retired, expected) in [(4, true, true), (1, true, false), (4, false, false)] {
        let mut w = Wire::default();
        let header = "VM id=4 epoch=12 name=timer2 state=";
        w.command("vm status timer2", &format!("\n{header}Ready vcpus=1 memory=256KiB\n  vcpu=0 owner=0 state=Started retired=false run=Runnable\nhv> {}\x1eH",tagged(12,"[vt] ready\n")));
        attach(&mut w, "timer2", 12);
        w.step(
            b'q',
            &format!(
                "{}\x1eHVM 4 epoch=12 Stopped\n\x1eDhv> ",
                tagged(12, "[vt] poweroff\n")
            ),
        );
        w.command("vm status timer2", &format!("\n{header}Stopped vcpus=1 memory=256KiB\n  vcpu=0 owner=0 state=Stopped retired={retired} run=Stopped\nVM {id} exit=Shutdown cause=0x0 pc=0x80200000 value=0x0\nhv> "));
        let mut c = w.controller();
        assert_eq!(
            Probe::start_vm(&mut c, 4, "timer2", 0)
                .unwrap()
                .shutdown(&mut c)
                .is_ok(),
            expected
        );
    }
}

#[test]
fn exactly_paired_receipts_and_each_workers_progress_are_required() {
    let base = HZ + 1;
    for (source_b, base_b, active_third, ok) in [
        (12, base, true, true),
        (99, base, true, false),
        (12, base + 1, true, false),
        (12, base, false, false),
    ] {
        let mut w = Wire::default();
        let workers = [("service", 21), ("busy2", 22), ("busy3", 23)].map(|(name, epoch)| Worker {
            name,
            epoch,
            owner: 0,
        });
        let mut probes = [(1, "control", 11), (4, "timer2", 12)].map(|(id, name, epoch)| Probe {
            id,
            name,
            epoch,
            owner: 0,
            cursor: 0,
            next_gen: 1,
            last_trap: 0,
        });
        for worker in &workers {
            worker_command(&mut w, worker, b'm', "[fair] measuring\n");
        }
        attach(&mut w, "control", 11);
        w.step(b'n', &tagged(11, "[vt] now=0000000000000001\n"));
        detach(&mut w, "");
        // Commands run in reverse order; outputs may arrive coalesced in the
        // opposite order, or from an epoch belonging to neither requested guest.
        queue(&mut w, "timer2", 12, base, "");
        let reports = tagged(source_b, &rows(base_b, 10)) + &tagged(11, &rows(base, 4));
        queue(&mut w, "control", 11, base, &reports);
        for (i, worker) in workers.iter().enumerate() {
            let n = if i == 2 && !active_third { 0 } else { 20_000 };
            let last = base + COUNT as u64 * STEP;
            worker_command(
                &mut w,
                worker,
                b'x',
                &format!(
                    "[fair] batches={n:016x} first=0000000000000000 last={last:016x} max_gap=0000000000002710\n"
                ),
            );
        }
        assert_eq!(
            round(&mut w.controller(), &mut probes, &workers, 1, true, true).is_ok(),
            ok
        );
    }
}
