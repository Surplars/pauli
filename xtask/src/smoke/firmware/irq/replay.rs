//! Replay a complete remote byte exchange without starting a guest.
//! Ownership: one UART script emits only the response to the current input.
//! Invariant: a valid guest ACK alone must never pass the native wake gate.
use super::*;
use crate::process::Output;
use std::{collections::VecDeque, time::Duration};

pub(super) struct Script {
    pub(super) steps: VecDeque<(u8, Vec<u8>)>,
    pub(super) next: Vec<u8>,
}
impl Script {
    pub(super) fn command(&mut self, command: &[u8], reply: &[u8]) {
        self.steps.extend(command.iter().map(|&b| (b, vec![b])));
        self.steps.push_back((b'\n', reply.to_vec()));
    }
}
impl Serial for Script {
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        let (expected, reply) = self.steps.pop_front().context("unexpected UART input")?;
        ensure!(bytes == [expected], "unexpected UART key");
        self.next = reply;
        Ok(())
    }
    fn receive(&mut self, _: Duration) -> Result<Output> {
        Ok(if self.next.is_empty() {
            Output::End
        } else {
            Output::Data(core::mem::take(&mut self.next))
        })
    }
}

#[test]
fn remote_ack_requires_a_new_worker_software_return_in_the_same_bracket() {
    const WAITING: &[u8] = b"\nVM id=2 epoch=2 name=service state=Ready vcpus=1\n  vcpu=0 owner=0 state=Started retired=false run=Waiting\nhv> ";
    const ARMED: &[u8] = b"\nworker-idle owner=0 parked=true software=10\nhv> ";
    for after in [10, 11] {
        // An earlier high count on the same host is not evidence for this byte.
        let mut wire = Script {
            steps: VecDeque::new(),
            next: b"worker-idle owner=0 parked=true software=99\n\x1eG00000002;[irq] ready\n\x1eH"
                .to_vec(),
        };
        wire.command(b"vm status service", WAITING);
        wire.command(b"vm capacity", ARMED);
        wire.command(b"vm attach service", b"\n\x1eA00000002;");
        wire.steps
            .push_back((b'A', format!("\x1eG00000002;{}", OLD[0].1).into_bytes()));
        wire.steps.push_back((0x1d, b"\x1eD\x1eHhv> ".to_vec()));
        let sample = format!("\nworker-idle owner=0 parked=true software={after}\nhv> ");
        wire.command(b"vm capacity", sample.as_bytes());
        wire.command(b"vm status service", WAITING);
        wire.command(b"vm capacity", sample.as_bytes());
        let mut c = Controller::new(wire, Duration::from_secs(1));
        let result = exchange(
            &mut c,
            1,
            2,
            Placement {
                boot: 1,
                service: 0,
            },
            &OLD[..1],
        );
        if after == 10 {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("no new native software wake")
            );
        } else {
            result.unwrap();
        }
    }
}

#[test]
fn clone_needs_its_own_first_byte_and_a_positive_destroy_reply() {
    for (ack_epoch, destroyed) in [(4, true), (3, true), (4, false)] {
        let mut wire = Script {
            steps: VecDeque::new(),
            next: format!("\x1eG00000003;{READY}{}{}\x1eH", NEW[0].1, NEW[1].1).into_bytes(),
        };
        wire.command(
            b"vm create service copy",
            b"\nVM 3 created: copy (stopped)\nhv> ",
        );
        wire.command(b"vm start copy", b"\nhv> ");
        wire.command(b"vm status copy",
            b"\nVM id=3 epoch=4 name=copy state=Ready vcpus=1\nhv> \x1eG00000004;[irq] ready\n\x1eH");
        for (name, epoch, (key, ack), recorded) in
            [("copy", 4, COPY, ack_epoch), ("service", 3, THIRD, 3)]
        {
            wire.command(
                format!("vm attach {name}").as_bytes(),
                format!("\n\x1eA{epoch:08x};").as_bytes(),
            );
            wire.steps
                .push_back((key, format!("\x1eG{recorded:08x};{ack}").into_bytes()));
            wire.steps.push_back((0x1d, b"\x1eD\x1eHhv> ".to_vec()));
        }
        wire.command(b"vm stop copy", b"\nVM 3 epoch=4 Stopped\nhv> ");
        wire.command(
            b"vm destroy copy",
            if destroyed {
                b"\nVM 3 destroyed\nhv> "
            } else {
                b"\nhv> "
            },
        );
        wire.command(
            b"vm status control",
            b"\nVM id=1 epoch=1 name=control state=Ready vcpus=1\nhv> ",
        );
        let mut c = Controller::new(wire, Duration::from_secs(1));
        c.wait(Source::Guest(3), 0, NEW[1].1).unwrap();
        let result = verify_clone(&mut c, 1, 3);
        assert_eq!(result.is_ok(), ack_epoch == 4 && destroyed, "{result:?}");
    }
}
