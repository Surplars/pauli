//! Byte-fragmented monitor/guest exchange. Replies become available only after
//! their triggering input, so one visit cannot consume a future reply.
use super::Probe;
use crate::{
    process::Output,
    smoke::controller::{Controller, Serial},
};
use anyhow::Result;
use std::{collections::VecDeque, fmt::Write, time::Duration};

pub(super) const CONTROL: &str = "\x1eG00000007;";
const OTHER: &str = "\x1eG00000006;";
const HOST: &str = "\x1eH";
pub(super) const READY: &str = "\nVM id=1 epoch=7 name=control state=Ready vcpus=1 memory=256KiB\n  vcpu=0 owner=0 state=Started retired=false run=Waiting\n";
pub(super) const STOPPED: &str = "\nVM id=1 epoch=7 name=control state=Stopped vcpus=1 memory=256KiB\nVM 1 exit=Shutdown cause=0x0 pc=0x80200100 value=0x0\n  vcpu=0 owner=0 state=Stopped retired=true run=Stopped\n";

#[derive(Default)]
pub(super) struct Wire {
    steps: VecDeque<(u8, Vec<u8>)>,
    pending: VecDeque<u8>,
}

impl Wire {
    pub(super) fn step(&mut self, key: u8, reply: &str) {
        self.steps.push_back((key, reply.as_bytes().to_vec()));
    }
    pub(super) fn command(&mut self, command: &str, reply: &str) {
        self.steps.extend(command.bytes().map(|b| (b, vec![b])));
        self.steps.push_back((b'\n', reply.as_bytes().to_vec()));
    }
    pub(super) fn boot(ready_source: &str, status: &str) -> Self {
        let mut wire = Self::default();
        wire.command(
            "vm status control",
            &format!("{status}hv> {ready_source}[vt] ready\n{HOST}"),
        );
        wire
    }
    fn attach(&mut self) {
        self.command("vm attach control", "\n\x1eA00000007;");
    }
    pub(super) fn capture(&mut self, source: &str, data: &str, on_detach: &str) {
        self.attach();
        self.steps
            .push_back((b't', format!("{source}{data}").into_bytes()));
        self.steps
            .push_back((0x1d, format!("{on_detach}\x1eD{HOST}hv> ").into_bytes()));
    }
    pub(super) fn shutdown(&mut self, extra: &str, status: &str) {
        self.attach();
        self.steps.push_back((
            b'q',
            format!("{CONTROL}[vt] poweroff\n{extra}{HOST}VM 1 epoch=7 Stopped\n\x1eDhv> ")
                .into_bytes(),
        ));
        self.command("vm status control", &format!("{status}hv> "));
    }
    pub(super) fn controller(self) -> Controller<Self> {
        Controller::new(self, Duration::from_secs(3))
    }
}
impl Serial for Wire {
    fn send(&mut self, input: &[u8]) -> Result<()> {
        // Assert rather than returning Err: negative cases must fail an evidence
        // check, not pass a test because the scripted command sequence was wrong.
        let (key, reply) = self.steps.pop_front().expect("unexpected input");
        assert_eq!(input, [key]);
        self.pending.extend(reply);
        Ok(())
    }
    fn receive(&mut self, _: Duration) -> Result<Output> {
        Ok(if self.pending.is_empty() {
            Output::End
        } else {
            Output::Data(self.pending.drain(..5.min(self.pending.len())).collect())
        })
    }
}

pub(super) fn samples(first: u64) -> String {
    let mut out = String::new();
    for n in first..first + 24 {
        let due = n * 20_000;
        let armed = due - 1;
        let trap = due + 1;
        writeln!(
            out,
            "[vt] gen={n:016x} due={due:016x} armed={armed:016x} trap={trap:016x} pc=post"
        )
        .unwrap();
    }
    out.push_str("[vt] done\n");
    out
}

#[test]
fn fragmented_visits_keep_one_guest_cursor_and_join_only_on_shutdown() -> Result<()> {
    let mut wire = Wire::boot(CONTROL, READY);
    wire.capture(CONTROL, &samples(1), "");
    wire.capture(CONTROL, &samples(25), "");
    wire.shutdown("", STOPPED);
    let mut c = wire.controller();
    let mut p = Probe::start(&mut c)?;
    assert_eq!(p.capture(&mut c)?.last_trap(), 24 * 20_000 + 1);
    assert_eq!(p.capture(&mut c)?.last_trap(), 48 * 20_000 + 1);
    p.shutdown(&mut c)
}

#[test]
fn old_epoch_and_host_text_cannot_supply_readiness_or_a_visit() -> Result<()> {
    for source in [OTHER, HOST] {
        assert!(Probe::start(&mut Wire::boot(source, READY).controller()).is_err());
        let mut wire = Wire::boot(CONTROL, READY);
        wire.capture(source, &samples(1), "");
        let mut c = wire.controller();
        assert!(Probe::start(&mut c)?.capture(&mut c).is_err());
    }
    Ok(())
}

#[test]
fn stale_missing_or_late_guest_data_cannot_be_the_next_complete_visit() -> Result<()> {
    let absent = samples(25).replace("[vt] done\n", "");
    let partial = samples(25).split_once('\n').unwrap().1.to_owned();
    for (second, extra) in [
        (samples(1), ""),
        (absent, ""),
        (partial, ""),
        (samples(25), "unsolicited\n"),
    ] {
        let mut wire = Wire::boot(CONTROL, READY);
        wire.capture(CONTROL, &samples(1), "");
        wire.capture(CONTROL, &second, extra);
        wire.shutdown("", STOPPED);
        let mut c = wire.controller();
        let mut p = Probe::start(&mut c)?;
        p.capture(&mut c)?;
        assert!(p.capture(&mut c).and_then(|_| p.shutdown(&mut c)).is_err());
    }
    Ok(())
}

#[test]
fn guest_poweroff_does_not_substitute_for_native_shutdown_and_retirement() -> Result<()> {
    for (extra, joined) in [
        ("", STOPPED.replace("exit=Shutdown", "exit=Fault")),
        ("", STOPPED.replace("retired=true", "retired=false")),
        ("", STOPPED.replace("epoch=7", "epoch=6")),
        ("late guest byte\n", STOPPED.to_owned()),
    ] {
        let mut wire = Wire::boot(CONTROL, READY);
        wire.capture(CONTROL, &samples(1), "");
        wire.shutdown(extra, &joined);
        let mut c = wire.controller();
        let mut p = Probe::start(&mut c)?;
        p.capture(&mut c)?;
        assert!(p.shutdown(&mut c).is_err());
    }
    Ok(())
}
