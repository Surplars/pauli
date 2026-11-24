//! Replay a fragmented wire through the production parser, including terminal EOF.
//! Ownership: one finite read-only UART stream; the probe must never require input.
//! Invariant: neither a success token nor a plausible duration substitutes for evidence.
use super::*;
use crate::process::Output;
use std::time::Duration;

struct Raw<'a>(std::slice::Chunks<'a, u8>);
impl Serial for Raw<'_> {
    fn send(&mut self, _: &[u8]) -> Result<()> {
        anyhow::bail!("native payload accepts no input")
    }
    fn receive(&mut self, _: Duration) -> Result<Output> {
        Ok(self
            .0
            .next()
            .map_or(Output::End, |b| Output::Data(b.to_vec())))
    }
}

fn transcript(sender: u8) -> String {
    let mut log = format!(
        "OpenSBI boot\r\nnative-ipi ready sender={sender} receiver={} hz=10000000\r\n",
        1 - sender
    );
    for (round, mode) in MODES.into_iter().enumerate() {
        for i in 0..BLOCK {
            let seq = round * BLOCK + i + 1;
            let (site, work) = match (mode, i % 2) {
                ("wait", 0) => ("wait-resume", 0),
                ("wait", _) => ("wait-armed", 0),
                _ => ("busy", seq),
            };
            log += &format!(
                "native-ipi seq={seq} mode={mode} send={} returned={} trap={} site={site} work={work}\r\n",
                seq * 100,
                seq * 100 + i % 3,
                seq * 100 + 1
            );
        }
    }
    log + "native-ipi complete\r\n"
}

fn check(log: &str) -> Result<()> {
    verify(&mut Controller::new(
        Raw(log.as_bytes().chunks(7)),
        Duration::from_secs(2),
    ))
}

#[test]
fn accepts_either_boot_hart_and_does_not_require_a_wfi_residency_claim() {
    check(&transcript(0)).unwrap();
    check(&transcript(1)).unwrap();
    check(&transcript(0).replace("wait-resume", "wait-armed")).unwrap();
    check(&transcript(1).replace("wait-armed", "wait-resume")).unwrap();
}

#[test]
fn rejects_wrong_peer_sequence_mode_or_trap_evidence() {
    let log = transcript(0);
    for (ok, bad) in [
        ("sender=0", "sender=1"),
        ("receiver=1", "receiver=2"),
        ("hz=10000000", "hz=0"),
        ("seq=9 mode=busy", "seq=9 mode=wait"),
        ("seq=31 mode=wait", "seq=32 mode=wait"),
        ("seq=2 mode=wait", "seq=1 mode=wait"),
        ("send=200 returned=201", "send=99 returned=201"),
        ("returned=100 trap=101", "returned=100 trap=99"),
        ("returned=100 trap=101", "returned=99 trap=101"),
        // A remote trap may precede SBI return, but the next send cannot.
        ("returned=100 trap=101", "returned=201 trap=101"),
        ("returned=100 trap=101", "returned=100 trap=201"),
        ("site=busy work=9", "site=busy work=0"),
        ("site=busy work=9", "site=wait-armed work=9"),
        ("site=wait-resume work=0", "site=wait-resume work=1"),
        ("site=wait-armed work=0", "site=unknown work=0"),
        ("site=wait-armed work=0", "site=wait-armed work=0 extra=1"),
    ] {
        assert!(check(&log.replace(ok, bad)).is_err(), "{bad}");
    }
}

#[test]
fn complete_requires_all_rows_and_no_later_report() {
    let full = transcript(0);
    let (head, _) = full.split_once("native-ipi seq=32 ").unwrap();
    assert!(check(&(head.to_owned() + "native-ipi complete\n")).is_err());
    assert!(check(full.trim_end()).is_err());
    assert!(check(head).is_err());
    assert!(check(&(full.clone() + "native-ipi complete\n")).is_err());
    assert!(check(&(full + "native-ipi FAIL\n")).is_err());
}
