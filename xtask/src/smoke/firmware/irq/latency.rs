//! Pair a virtual UART RX rise, the owner's observation and the first VS trap in one TIME domain.
//! Ownership: one byte in flight; host and guest records are scoped to the same service epoch.
//! Invariants: a wait sample also requires a disarmed remote WFI and a fresh SSIP-only return.
use super::{Controller, Placement, Serial, Source, cpu, fresh_window, parked, wake_delta, worker};
use anyhow::{Context, Result, ensure};
mod capture;
mod peers;
mod records;
mod timeline;
pub(in crate::smoke) use capture::Capture;
use capture::Population;
use records::{native_ack, owner_ticks, rise_ticks, trap_ticks};
use timeline::Sample;

const READY: &str = "[lat] ready\n";
const PER_ROUND: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Wait,
    Busy,
}
impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Wait => "wait",
            Self::Busy => "busy",
        }
    }
    fn key(self) -> u8 {
        self.name().as_bytes()[0]
    }
}

pub(in crate::smoke::firmware) fn verify(
    c: &mut Controller,
    scale: bool,
    capture: &mut Capture,
) -> Result<()> {
    let place = Placement::parse(c.log(Source::Host), true)?;
    let control = c.status("control", "Ready")?;
    cpu(&c.log(Source::Host)[c.last_command()..], place.boot)?;
    let hz: u64 = super::super::line(
        c,
        Source::Guest(control),
        &mut 0,
        "[rtos] scheduler ready hz=",
    )?
    .parse()?;
    ensure!(hz != 0, "invalid TIME frequency");
    let before = fresh_window(c, control)?;
    let mut run = Epoch::start(c, place)?;
    let old = run.id;
    if scale {
        compare_waiters(c, &mut run, control, hz, capture)?;
    } else {
        run.score(
            c,
            PER_ROUND,
            hz,
            Population {
                reserved: 0,
                waiting: 0,
            },
            capture,
        )?;
    }
    c.command("vm stop service")?;
    c.terminal(2, old, "Stopped")?;
    c.command("vm start service")?;
    let mut fresh = Epoch::start(c, place)?;
    ensure!(fresh.id != old, "cold restart reused latency epoch {old}");
    // Do not mix the unscored cold-start checks with the ABBA distributions.
    for mode in [Mode::Wait, Mode::Busy] {
        fresh.select(c, mode)?;
        fresh.sample(c, mode)?;
    }
    ensure!(
        fresh_window(c, control)? > before && c.status("control", "Ready")? == control,
        "RTOS peer did not survive service restart"
    );
    println!(
        "firmware IRQ latency: cold restart {old}->{} verified wait/busy; control epoch={control}",
        fresh.id
    );
    Ok(())
}

/// Reserve once so the directory, placements, active service and its epoch do
/// not change between phases. Repeat the lighter loads to expose time drift.
fn compare_waiters(
    c: &mut Controller,
    run: &mut Epoch,
    control: u32,
    hz: u64,
    capture: &mut Capture,
) -> Result<()> {
    const MAX: usize = 8;
    let mut peers = peers::Peers::reserve(c, run.place.service, control, run.id, MAX)?;
    for (phase, waiting) in [0, MAX / 2, MAX, MAX / 2, 0].into_iter().enumerate() {
        peers.resize(c, waiting)?;
        peers.check(c)?;
        println!(
            "firmware IRQ scale: phase={} reserved={MAX} waiting={waiting} stopped={} epoch={} owner={}",
            phase + 1,
            MAX - waiting,
            run.id,
            run.place.service
        );
        // Give each load the same total of 16 samples per mode across its visits.
        run.score(
            c,
            if waiting == MAX {
                PER_ROUND
            } else {
                PER_ROUND / 2
            },
            hz,
            Population {
                reserved: MAX,
                waiting,
            },
            capture,
        )?;
        peers.check(c)?;
    }
    peers.remove(c)
}

struct Epoch {
    id: u32,
    place: Placement,
    inputs: u64, // Host counts every RX rise, guest counts only sample commands.
    samples: u64,
    last_tick: u64,
    cursor: usize,
}
impl Epoch {
    fn score(
        &mut self,
        c: &mut Controller,
        per_round: usize,
        hz: u64,
        population: Population,
        capture: &mut Capture,
    ) -> Result<()> {
        let mut events = Vec::with_capacity(4 * per_round);
        for mode in [Mode::Wait, Mode::Busy, Mode::Busy, Mode::Wait] {
            self.select(c, mode)?;
            for _ in 0..per_round {
                events.push((mode, self.sample(c, mode)?));
            }
        }
        capture.record(population, self.id, self.place.service, hz, events)
    }
    fn start<T: Serial>(c: &mut Controller<T>, place: Placement) -> Result<Self> {
        let id = c.status("service", "Ready")?;
        cpu(&c.log(Source::Host)[c.last_command()..], place.service)?;
        let src = Source::Guest(id);
        c.wait(src, 0, READY)?;
        ensure!(
            c.log(src) == READY.as_bytes(),
            "unexpected initial latency transcript"
        );
        Ok(Self {
            id,
            place,
            inputs: 0,
            samples: 0,
            last_tick: 0,
            cursor: READY.len(),
        })
    }

    fn select<T: Serial>(&mut self, c: &mut Controller<T>, mode: Mode) -> Result<()> {
        let (row, _) = self.exchange(c, mode.key())?;
        ensure!(
            row == format!("[lat] {}", mode.name()),
            "wrong mode ACK: {row}"
        );
        Ok(())
    }

    fn sample<T: Serial>(&mut self, c: &mut Controller<T>, mode: Mode) -> Result<Sample> {
        // Status requests already yield to the serial controller. Recheck a racing park
        // directly instead of charging each sample a full peer RTOS reporting interval.
        let before = if mode == Mode::Wait {
            Some(
                parked(c, None, self.id, self.place)?
                    .context("wait latency requires a remote owner")?,
            )
        } else {
            None
        };
        let (row, rise) = self.exchange(c, b'p')?;
        self.samples += 1;
        let trap = trap_ticks(row, self.samples, mode)?;
        // exchange's fresh status reply is still current. A later vm capacity/status
        // command, a prior input or the other hart must not supply the middle timestamp.
        let status = &c.log(Source::Host)[c.last_command()..];
        let owner = owner_ticks(status, self.id, self.inputs, self.place.service)?;
        let ack = native_ack(status, self.id, self.inputs, self.place.service)?;
        let sample = Sample::new(self.inputs, self.samples, rise, owner, trap)?.with_ack(ack)?;
        self.last_tick = trap;
        if let Some(before) = before {
            wake_delta(before, worker(c, self.place.service)?.1)?;
        }
        Ok(sample)
    }

    fn exchange<'a, T: Serial>(
        &mut self,
        c: &'a mut Controller<T>,
        key: u8,
    ) -> Result<(&'a str, u64)> {
        let src = Source::Guest(self.id);
        ensure!(
            self.cursor == c.log(src).len(),
            "unsolicited latency output"
        );
        c.attach("service", self.id)?;
        c.guest(self.id, key, "\n")?;
        c.detach()?;
        ensure!(
            c.status("service", "Ready")? == self.id,
            "service epoch changed"
        );
        self.inputs += 1;
        let rise = rise_ticks(
            &c.log(Source::Host)[c.last_command()..],
            self.id,
            self.inputs,
            key,
        )?;
        ensure!(
            rise >= self.last_tick,
            "UART rise predates prior observation"
        );
        self.last_tick = rise;
        let log = c.log(src);
        let row = std::str::from_utf8(&log[self.cursor..])?
            .strip_suffix('\n')
            .context("partial guest IRQ record")?;
        self.cursor = log.len();
        Ok((row, rise))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pairing_rejects_stale_foreign_duplicate_and_unproven_mode() {
        let row = b"uart-rise epoch=7 seq=2 byte=112 ticks=11\nhv> ";
        assert_eq!(rise_ticks(row, 7, 2, b'p').unwrap(), 11);
        for (epoch, seq, key) in [(2, 2, b'p'), (7, 1, b'p'), (7, 2, b'b')] {
            assert!(rise_ticks(row, epoch, seq, key).is_err());
        }
        assert!(rise_ticks(&[row.as_slice(), b"\n", row].concat(), 7, 2, b'p').is_err());
        let owner = "uart-owner epoch=7 seq=2 vcpu=0 owner=1 ticks=12\n";
        assert_eq!(owner_ticks(owner.as_bytes(), 7, 2, 1).unwrap(), 12);
        for wrong in [
            owner.replace("epoch=7", "epoch=6"),
            owner.replace("seq=2", "seq=1"),
            owner.replace("vcpu=0", "vcpu=1"),
            owner.replace("owner=1", "owner=0"),
            owner.repeat(2),
        ] {
            assert!(owner_ticks(wrong.as_bytes(), 7, 2, 1).is_err());
        }
        let trap = "[lat] sample seq=0000000000000001 mode=busy ticks=000000000000000c";
        assert_eq!(trap_ticks(trap, 1, Mode::Busy).unwrap(), 12);
        assert!(trap_ticks(trap, 2, Mode::Busy).is_err());
        assert!(trap_ticks(trap, 1, Mode::Wait).is_err());
        assert!(trap_ticks(&trap[..trap.len() - 1], 1, Mode::Busy).is_err());
        assert!(trap_ticks(&format!("{trap}\n{trap}"), 1, Mode::Busy).is_err());
    }

    #[test]
    fn command_replay_requires_a_causal_time_pair_and_a_native_wait_return() -> Result<()> {
        use super::super::replay::Script;
        use std::{collections::VecDeque, time::Duration};
        use timeline::{Ack, AckPath};
        const STATUS: &[u8] = b"\nVM id=2 epoch=7 name=service state=Ready vcpus=1\n  vcpu=0 owner=0 state=Started retired=false run=Waiting\n";
        let place = Placement {
            boot: 1,
            service: 0,
        };
        for (mode, seen, trapped, wakes, valid) in [
            (Mode::Wait, Some(104), 112, 6, true),
            (Mode::Wait, Some(104), 112, 5, false),
            (Mode::Wait, Some(104), 98, 6, false),
            (Mode::Busy, Some(104), 112, 5, true),
            (Mode::Busy, Some(99), 112, 5, false),
            (Mode::Busy, Some(113), 112, 5, false),
            (Mode::Busy, None, 112, 5, false),
        ] {
            let mut wire = Script {
                steps: VecDeque::new(),
                next: Vec::new(),
            };
            let enter = |wire: &mut Script| wire.command(b"vm attach service", b"\n\x1eA00000007;");
            let exit = |wire: &mut Script| wire.steps.push_back((0x1d, b"\x1eD\x1eHhv> ".to_vec()));
            wire.command(
                b"vm status service",
                &[STATUS, b"uart-owner epoch=7 seq=2 vcpu=0 owner=0 ticks=104\nuart-ack epoch=7 seq=1 vcpu=0 owner=0 path=dispatch-ack send=20 ticks=23 returned=21\nhv> \x1eG00000007;[lat] ready\n\x1eH"].concat(),
            );
            enter(&mut wire);
            wire.steps.push_back((
                mode.key(),
                format!("\x1eG00000007;[lat] {}\n", mode.name()).into_bytes(),
            ));
            exit(&mut wire);
            wire.command(
                b"vm status service",
                &[
                    STATUS,
                    format!("uart-rise epoch=7 seq=1 byte={} ticks=40\nhv> ", mode.key())
                        .as_bytes(),
                ]
                .concat(),
            );
            if mode == Mode::Wait {
                wire.command(b"vm status service", &[STATUS, b"hv> "].concat());
                wire.command(
                    b"vm capacity",
                    b"\nworker-idle owner=0 parked=true software=5\nhv> ",
                );
            }
            enter(&mut wire);
            wire.steps.push_back((b'p', format!("\x1eG00000007;[lat] sample seq=0000000000000001 mode={} ticks={trapped:016x}\n", mode.name()).into_bytes()));
            exit(&mut wire);
            let owner_row = seen
                .map(|ticks| format!("uart-owner epoch=7 seq=2 vcpu=0 owner=0 ticks={ticks}\n"))
                .unwrap_or_default();
            wire.command(
                b"vm status service",
                &[
                    STATUS,
                    b"uart-rise epoch=7 seq=2 byte=112 ticks=100\n",
                    owner_row.as_bytes(),
                    if mode == Mode::Busy { b"uart-ack epoch=7 seq=2 vcpu=0 owner=0 path=guest-exit send=101 ticks=102 returned=115\n" } else { b"" },
                    b"hv> ",
                ]
                .concat(),
            );
            if mode == Mode::Wait {
                wire.command(
                    b"vm capacity",
                    format!("\nworker-idle owner=0 parked=true software={wakes}\nhv> ").as_bytes(),
                );
            }
            let mut c = Controller::new(wire, Duration::from_secs(2));
            let mut e = Epoch::start(&mut c, place)?;
            e.select(&mut c, mode)?;
            let measured = e.sample(&mut c, mode);
            if valid {
                let ack = (mode == Mode::Busy).then_some(Ack {
                    path: AckPath::GuestExit,
                    send: 101,
                    ticks: 102,
                    returned: 115, // Sender may still be in SBI when VS has already trapped.
                });
                assert_eq!(measured?, Sample::new(2, 1, 100, 104, 112)?.with_ack(ack)?);
            } else {
                assert!(
                    measured.is_err(),
                    "{mode:?} trapped={trapped} wakes={wakes}"
                );
            }
        }
        Ok(())
    }
}
