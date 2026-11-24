//! Replay peer creation, parking and retirement without starting QEMU.
//! Ownership: the test controls one serial script; no worker or lock is shared.
//! Invariant: only the matching host status and quiet guest epoch prove a parked peer.
use super::*;
use crate::smoke::firmware::irq::replay::Script;
use std::time::Duration;

const FRESH: &str = "Started retired=true run=Runnable";
const WAIT: &str = "Started retired=false run=Waiting";
const JOINED: &str = "Stopped retired=true run=Stopped";

fn script() -> Script {
    Script {
        steps: Default::default(),
        next: Vec::new(),
    }
}

fn cmd(s: &mut Script, cmd: &str, output: &str) {
    s.command(cmd.as_bytes(), format!("\n{output}hv> ").as_bytes());
}

fn status(id: u32, epoch: u32, state: &str, cpu: &str) -> String {
    format!(
        "VM id={id} epoch={epoch} name=idle{id} state={state} vcpus=1 memory=32KiB\n  vcpu=0 owner=1 state={cpu}\n"
    )
}

fn capacity(count: usize) -> String {
    format!("VM resources: allocated={count} id-limit=32 vcpus/VM=1..8\n")
}

#[test]
fn mirrored_sweep_retains_slots_epochs_and_waiters_until_final_destruction() -> Result<()> {
    let mut s = script();
    for id in 3..=4 {
        cmd(
            &mut s,
            &format!("vm create service idle{id}"),
            &format!("VM {id} created: idle{id} (stopped)\n"),
        );
        cmd(
            &mut s,
            &format!("vm status idle{id}"),
            &status(id, id + 4, "Stopped", FRESH),
        );
    }
    let mut live = 0;
    let mut peak = 0;
    for n in [0, 1, 2, 1, 0] {
        while live < n {
            let id = 3 + live;
            let epoch = id + 4;
            cmd(
                &mut s,
                &format!("vm start idle{id}"),
                &format!("VM {id} started\n\x1eG{epoch:08x};{READY}\x1eH"),
            );
            let command = format!("vm status idle{id}");
            // First sample precedes WFI. No tick-based sleep is needed for the second.
            cmd(
                &mut s,
                &command,
                &status(id, epoch, "Ready", "Started retired=false run=Runnable"),
            );
            cmd(&mut s, &command, &status(id, epoch, "Ready", WAIT));
            live += 1;
        }
        peak = peak.max(live);
        while live > n {
            let id = 2 + live;
            cmd(
                &mut s,
                &format!("vm stop idle{id}"),
                &format!("VM {id} stop requested\nVM {id} epoch={} Stopped\n", id + 4),
            );
            live -= 1;
        }
        let mut snapshot = String::new();
        for i in 0..2 {
            let (state, cpu) = if i < n {
                ("Ready", WAIT)
            } else if i < peak {
                ("Stopped", JOINED)
            } else {
                ("Stopped", FRESH)
            };
            snapshot += &status(i + 3, i + 7, state, cpu);
        }
        cmd(&mut s, "vm capacity", &capacity(4));
        cmd(&mut s, "vm status", &snapshot);
    }
    cmd(&mut s, "vm destroy idle4", "VM 4 destroyed\n");
    cmd(&mut s, "vm destroy idle3", "VM 3 destroyed\n");
    cmd(&mut s, "vm capacity", &capacity(2));

    let mut c = Controller::new(s, Duration::from_secs(1));
    let mut peers = Peers::reserve(&mut c, 1, 1, 2, 2)?;
    for n in [0, 1, 2, 1, 0] {
        peers.resize(&mut c, n)?;
        peers.check(&mut c)?;
    }
    peers.remove(&mut c)
}

#[test]
fn require_quiet_epoch_scoped_guest_and_exact_unique_owner_row() {
    let correct = status(3, 7, "Ready", WAIT);
    let valid = capacity(3);
    for (resources, row, epoch, guest) in [
        (capacity(13), correct.clone(), 7, READY),
        (valid.clone() + &capacity(3), correct.clone(), 7, READY),
        (valid.clone(), correct.clone(), 6, READY),
        (
            valid.clone(),
            correct.clone(),
            7,
            "[lat] ready\n[lat] wait\n",
        ),
        (
            valid.clone(),
            correct.replace("epoch=7", "epoch=3"),
            7,
            READY,
        ),
        (
            valid.clone(),
            correct.replace("owner=1", "owner=0"),
            7,
            READY,
        ),
        (
            valid.clone(),
            correct.replace("Waiting", "Runnable"),
            7,
            READY,
        ),
        (valid.clone(), correct.replace("32KiB", "256KiB"), 7, READY),
        (valid.clone(), correct.clone() + &correct, 7, READY),
    ] {
        let mut s = script();
        cmd(&mut s, "vm capacity", &resources);
        cmd(
            &mut s,
            "vm status",
            &(row + &format!("\x1eG{epoch:08x};{guest}\x1eH")),
        );
        let peers = Peers {
            owner: 1,
            clones: vec![Peer {
                id: 3,
                name: "idle3".into(),
                epoch: 7,
                booted: true,
            }],
            started: 1,
        };
        assert!(
            peers
                .check(&mut Controller::new(s, Duration::from_secs(1)))
                .is_err()
        );
    }
}
