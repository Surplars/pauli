use super::*;
use crate::process::Output;
use std::{collections::VecDeque, time::Duration};

const VM: &str = "\nVM id=1 epoch=7 name=control state=Ready vcpus=1 memory=256KiB\n";
const CPU: &str = "  vcpu=0 owner=0 state=Started retired=false run=Waiting\n";
const PROMPT: &str = "hv> ";
const PERIOD: &str =
    "\x1eG00000007;[rtos] samples=32 max_late_ticks=2 missed=0 window_max_ticks=1\x1eN\x1eH";

// The reply starts only after acknowledging each key and receiving the LF;
// separate receive chunks allow the next periodic report to follow the prompt.
struct Wire {
    line: Vec<u8>,
    replies: VecDeque<Vec<String>>,
    pending: VecDeque<String>,
}
impl Serial for Wire {
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        let [b] = *bytes else {
            bail!("expected one key")
        };
        ensure!(self.pending.is_empty(), "unread serial output");
        if b == b'\n' {
            ensure!(self.line == b"vm status control", "wrong command");
            self.line.clear();
            self.pending
                .extend(self.replies.pop_front().context("unexpected retry")?);
        } else {
            self.line.push(b);
            self.pending.push_back(char::from(b).to_string());
        }
        Ok(())
    }
    fn receive(&mut self, _: Duration) -> Result<Output> {
        Ok(self
            .pending
            .pop_front()
            .map_or(Output::End, |s| Output::Data(s.into_bytes())))
    }
}
fn wire(replies: Vec<Vec<String>>) -> Controller<Wire> {
    Controller::new(
        Wire {
            line: vec![],
            replies: replies.into(),
            pending: VecDeque::new(),
        },
        Duration::from_secs(2),
    )
}

#[test]
fn accepted_proof_is_fresh_and_from_the_host_before_the_prompt() -> Result<()> {
    let valid = format!("{VM}uart-rise epoch=7 seq=1 byte=115 ticks=814\n{CPU}{PROMPT}");
    for missing in [
        format!("{VM}{PROMPT}"),
        format!("\n\x1eG00000007;{VM}{CPU}\x1eH{PROMPT}"),
        format!("{VM}{PROMPT}\n{CPU}"),
    ] {
        let mut c = wire(vec![vec![valid.clone()], vec![missing]]);
        control_waiting(&mut c, 7)?;
        assert!(control_waiting(&mut c, 7).is_err());
    }
    Ok(())
}

#[test]
fn incorrect_identity_lifecycle_or_duplicate_rows_fail_without_retry() {
    let valid = format!("{VM}{CPU}{PROMPT}");
    for (from, to) in [
        ("id=1", "id=2"),
        ("epoch=7", "epoch=6"),
        ("name=control", "name=service"),
        ("state=Ready", "state=Stopped"),
        ("vcpus=1", "vcpus=2"),
        ("vcpu=0", "vcpu=1"),
        ("owner=0", "owner=1"),
        ("retired=false", "retired=true"),
        ("run=Waiting", "run=Blocked"),
        ("run=Waiting", "run=Starting"),
        ("run=Waiting", "run=Waiting?"),
        (VM, &format!("{VM}{VM}")),
        (CPU, &format!("{CPU}{CPU}")),
    ] {
        let mut c = wire(vec![vec![valid.replace(from, to)]]);
        let err = control_waiting(&mut c, 7).unwrap_err().to_string();
        assert!(
            err.starts_with("unexpected control") || err.starts_with("duplicate"),
            "{err}"
        );
    }
}

#[test]
fn transient_state_resamples_until_a_new_host_waiting_observation() -> Result<()> {
    let waiting = format!("{VM}{CPU}{PROMPT}");
    let starting = waiting
        .replace("state=Started", "state=StartPending")
        .replace("run=Waiting", "run=Starting");
    let runnable = waiting.replace("run=Waiting", "run=Runnable");
    let mut c = wire(vec![
        vec![starting, PERIOD.into()],
        vec![runnable],
        vec![waiting],
    ]);
    control_waiting(&mut c, 7)
}

#[test]
fn runnable_alone_or_a_stale_or_foreign_report_cannot_pass() {
    let waiting = format!("{VM}{CPU}{PROMPT}");
    let seen = format!("{}{PERIOD}", waiting.replace("run=Waiting", "run=Runnable"));
    let mut c = wire(vec![
        vec![seen, PERIOD.replace("00000007", "00000008")],
        vec![waiting],
    ]);
    let err = control_waiting(&mut c, 7).unwrap_err();
    assert!(format!("{err:#}").contains("waiting for Guest(7): [rtos] samples="));
}
