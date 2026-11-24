use super::*;
use std::fmt::Write;

const HELLO: &str = "[mb] ready cpus=2 hz=10000000\n";

fn report() -> String {
    let mut out = HELLO.to_owned();
    for n in 1..=N {
        for (i, op) in OPS.iter().enumerate().rev() {
            writeln!(out, "[mb] sample op={op} seq={n} ticks={}", i * 100 + n).unwrap();
        }
    }
    out.push_str("[mb] receipts ipi=28\n[mb] done\n");
    out
}

#[test]
fn separates_interleaved_operations_and_keeps_acquisition_order() {
    let data = parse(report().as_bytes(), HELLO, false).unwrap();
    for (i, samples) in data.into_iter().enumerate() {
        assert_eq!(
            samples.map(|w| w.ticks),
            std::array::from_fn(|j| (100 * i + j + 1) as u64)
        );
    }
    let descending = std::array::from_fn(|i| (N - i) as u64);
    assert_eq!(ranks(descending), [12, 23, 24]);
}

#[test]
fn rejects_missing_duplicate_out_of_sequence_and_invalid_records() {
    let valid = report();
    for (old, new) in [
        ("ready cpus=2", "ready cpus=1"),
        ("op=vma seq=2", "op=vma seq=1"),
        ("op=ipi seq=2", "op=ipi seq=3"),
        ("op=vma seq=1", "op=ipi seq=1"),
        ("op=ipi seq=1", "op=ipi seq=+1"),
        ("op=fencei", "op=fence"),
        ("ticks=201", "ticks=18446744073709551616"),
        ("ticks=201", "ticks=+201"),
        ("[mb] sample op=ipi seq=1 ticks=1\n", ""),
        ("receipts ipi=28", "receipts ipi=24"),
        ("receipts ipi=28\n", "receipts ipi=28"),
        ("[mb] done\n", "extra line\n[mb] done\n"),
        ("[mb] done\n", "[mb] done"),
        ("[mb] done\n", "[mb] done\nlate line\n"),
    ] {
        assert!(
            parse(valid.replacen(old, new, 1).as_bytes(), HELLO, false).is_err(),
            "{old:?} -> {new:?}"
        );
    }
}

#[test]
fn boot_hart_and_timebase_must_match_the_requested_machine() {
    let s = "[HV] QEMU virt: 2 harts, boot 1, 10000000 Hz\n";
    assert_eq!(native_board(s.as_bytes(), 2).unwrap(), (1, 10_000_000));
    assert_eq!(
        native_board(s.replace("boot 1", "boot 0").as_bytes(), 2).unwrap(),
        (0, 10_000_000)
    );
    let one = s.replace("2 harts, boot 1", "1 harts, boot 0");
    assert_eq!(native_board(one.as_bytes(), 1).unwrap(), (0, 10_000_000));
    assert!(native_board(one.as_bytes(), 2).is_err());
    assert!(native_board(s.as_bytes(), 1).is_err());
    assert!(native_board(one.replace("boot 0", "boot 1").as_bytes(), 1).is_err());
    for bad in [
        s.repeat(2),
        s.replace("boot 1", "boot 2"),
        s.replace("2 harts", "1 hart"),
        s.replace("10000000", "0"),
    ] {
        assert!(native_board(bad.as_bytes(), 2).is_err(), "{bad}");
    }
}

const LIVE: &str = "VM id=1 epoch=7 name=alpha state=Ready vcpus=2 memory=64MiB\n  vcpu=0 owner=0 state=Started retired=false run=Runnable\n  vcpu=1 owner=1 state=Started retired=false run=Waiting\nhv> ";
const JOINED: &str = "VM id=1 epoch=7 name=alpha state=Stopped vcpus=2 memory=64MiB\nVM 1 exit=Shutdown cause=0x0 pc=0x82000000 value=0x0\n  vcpu=0 owner=0 state=Stopped retired=true run=Stopped\n  vcpu=1 owner=1 state=Stopped retired=true run=Stopped\nhv> ";
fn shared(status: &str) -> String {
    status.replace("vcpu=1 owner=1", "vcpu=1 owner=0")
}

#[test]
fn requires_actual_two_cpu_placement_in_current_epoch_and_guest_shutdown() {
    let dedicated = [0, 1];
    owners(LIVE.as_bytes(), 7, false, dedicated).unwrap();
    owners(shared(LIVE).as_bytes(), 7, false, [0, 0]).unwrap();
    assert!(owners(LIVE.as_bytes(), 7, false, [0, 0]).is_err());
    assert!(owners(shared(LIVE).as_bytes(), 7, false, dedicated).is_err());
    let first_slice = LIVE.replace(
        "Started retired=false run=Waiting",
        "StartPending retired=false run=Starting",
    );
    owners(first_slice.as_bytes(), 7, false, dedicated).unwrap();
    owners(JOINED.as_bytes(), 7, true, dedicated).unwrap();
    assert!(owners(LIVE.as_bytes(), 8, false, dedicated).is_err());
    for (old, new) in [
        ("owner=1", "owner=0"),
        ("owner=0", "owner=2"),
        ("vcpu=1", "vcpu=0"),
        ("vcpus=2", "vcpus=1"),
        ("retired=false", "retired=true"),
    ] {
        assert!(
            owners(LIVE.replacen(old, new, 1).as_bytes(), 7, false, dedicated).is_err(),
            "{old} -> {new}"
        );
    }
    for (old, new) in [
        ("retired=true", "retired=false"),
        ("exit=Shutdown", "exit=Fault"),
        ("VM 1 exit=", "VM 2 exit="),
    ] {
        assert!(
            owners(JOINED.replacen(old, new, 1).as_bytes(), 7, true, dedicated).is_err(),
            "{old} -> {new}"
        );
    }
}

/// One input byte at a time as required by the host; split output frames across reads.
/// Guest commands are not echoed and fresh status is available only after shutdown.
struct Wire {
    steps: std::collections::VecDeque<(u8, Vec<u8>)>,
    out: std::collections::VecDeque<u8>,
}

impl Wire {
    fn command(&mut self, name: &str, reply: &str) {
        self.steps.extend(name.bytes().map(|b| (b, vec![b])));
        self.steps.push_back((b'\n', reply.as_bytes().to_vec()));
    }
}

impl super::super::controller::Serial for Wire {
    fn send(&mut self, input: &[u8]) -> Result<()> {
        let (expected, reply) = self.steps.pop_front().context("unexpected input")?;
        ensure!(input == [expected], "wrong mailbox command sequence");
        self.out.extend(reply);
        Ok(())
    }

    fn receive(&mut self, _: std::time::Duration) -> Result<crate::process::Output> {
        Ok(if self.out.is_empty() {
            crate::process::Output::End
        } else {
            crate::process::Output::Data(self.out.drain(..17.min(self.out.len())).collect())
        })
    }
}

fn replay(sample_epoch: u32, status: &str) -> Result<()> {
    wire_exchange(2, sample_epoch, status, &report(), None)
}

fn wire_exchange(
    host_cpus: u8,
    sample_epoch: u32,
    status: &str,
    samples: &str,
    fence_reply: Option<&str>,
) -> Result<()> {
    let (key, workload) = if fence_reply.is_some() {
        (b'd', Workload::FenceSplit)
    } else if samples.starts_with(&format!("{HELLO}[mb] handoff")) {
        (b'i', Workload::IpiHandoff)
    } else {
        (b'g', Workload::RoundTrip)
    };
    let mut wire = Wire {
        steps: Default::default(),
        out: format!("[HV] QEMU virt: {host_cpus} harts, boot 0, 10000000 Hz\nhv> ")
            .bytes()
            .collect(),
    };
    let live = if host_cpus == 1 {
        shared(LIVE)
    } else {
        LIVE.to_owned()
    };
    // First status is allowed to precede the worker's ready record.
    wire.command(
        "vm status alpha",
        &format!("\n{live}\x1eG00000007;{HELLO}\x1eH"),
    );
    wire.command("vm status alpha", &format!("\n{live}"));
    wire.command("vm attach alpha", "\n\x1eA00000007;");
    let samples = samples.strip_prefix(HELLO).unwrap();
    wire.steps.push_back((
        key,
        format!("\x1eG{sample_epoch:08x};{samples}").into_bytes(),
    ));
    wire.steps.push_back((
        b'q',
        format!("{OFF}\x1eHVM 1 epoch=7 Stopped\n\x1eDhv> ").into_bytes(),
    ));
    wire.command("vm status alpha", &format!("\n{status}"));
    if let Some(reply) = fence_reply {
        wire.command("vm fences alpha", reply);
    }
    let mut c = Controller::new(wire, std::time::Duration::from_secs(2));
    c.wait(Source::Host, 0, "hv> ")?;
    verify(&mut c, host_cpus, workload)
}

#[test]
fn handoff_must_be_from_the_joined_epoch_and_actual_placement() {
    let mut guest = HELLO.to_owned();
    for n in 1..=64 {
        let mode = if (17..=48).contains(&n) {
            "wfi"
        } else {
            "busy"
        };
        writeln!(
            guest,
            "[mb] handoff gen={n} mode={mode} begin={n} received={n} end={n}"
        )
        .unwrap();
    }
    guest.push_str("[mb] receipts ipi=64 wfi_returns=32\n[mb] done\n");
    let joined = shared(JOINED);
    wire_exchange(1, 7, &joined, &guest, None).unwrap();
    assert!(wire_exchange(1, 6, &joined, &guest, None).is_err());
    assert!(wire_exchange(1, 7, JOINED, &guest, None).is_err());
    assert!(
        wire_exchange(
            2,
            7,
            &JOINED.replace("exit=Shutdown", "exit=Fault"),
            &guest,
            None
        )
        .is_err()
    );
}

/// The target's replies need not arrive in generation order. Guest windows do:
/// there is only one sender, and it cannot issue the next call until the last returns.
pub(super) fn split_fixture(owner: u8) -> (String, String) {
    let mut guest = HELLO.to_owned();
    let mut host = String::from("\n");
    for round in 0..(WARMUP + N) {
        for offset in 0..3 {
            let op = (round + offset) % 3;
            let gen_id = round + 1;
            let start = (3 * round + offset) * 100 + 100;
            if round >= WARMUP {
                writeln!(
                    guest,
                    "[mb] sample op={} seq={} ticks=40 start={start}",
                    OPS[op],
                    gen_id - WARMUP
                )
                .unwrap();
            }
            if op != 0 {
                writeln!(
                    host,
                    "fence epoch=7 target=1 owner={owner} requester=0 op={} gen={gen_id} ticks={} path={}",
                    OPS[op],
                    start + 15,
                    if gen_id.is_multiple_of(2) { "direct" } else { "deferred" }
                )
                .unwrap();
            }
        }
    }
    guest.push_str("[mb] receipts ipi=28\n[mb] done\n");
    host.push_str("hv> ");
    (guest, host)
}

#[test]
fn split_command_uses_only_clean_post_shutdown_native_output() {
    let (guest, host) = split_fixture(1);
    wire_exchange(2, 7, JOINED, &guest, Some(&host)).unwrap();
    // Valid-looking lines from the PREVIOUS host command must not count.
    let old = JOINED.replace("hv> ", &host);
    assert!(wire_exchange(2, 7, &old, &guest, Some("\nhv> ")).is_err());
}

#[test]
fn one_native_hart_runs_both_modes_without_accepting_dedicated_owners() {
    let joined = shared(JOINED);
    wire_exchange(1, 7, &joined, &report(), None).unwrap();
    let (guest, host) = split_fixture(0);
    wire_exchange(1, 7, &joined, &guest, Some(&host)).unwrap();
    assert!(wire_exchange(1, 7, JOINED, &guest, Some(&host)).is_err());
    let (_, wrong_target) = split_fixture(1);
    assert!(wire_exchange(1, 7, &joined, &guest, Some(&wrong_target)).is_err());
}

#[test]
fn full_fragmented_exchange_needs_current_guest_and_joined_native_evidence() {
    replay(7, JOINED).unwrap();
    // A complete, valid measurement delivered under an old epoch is not ours.
    let stale = replay(6, JOINED).unwrap_err();
    assert!(format!("{stale:#}").contains("waiting for Guest(7)"));
    let fault = replay(7, &JOINED.replace("exit=Shutdown", "exit=Fault")).unwrap_err();
    assert!(fault.to_string().contains("did not shut down cleanly"));
    let swapped = JOINED
        .replace("vcpu=0 owner=0", "vcpu=0 owner=1")
        .replace("vcpu=1 owner=1", "vcpu=1 owner=0");
    let migrated = replay(7, &swapped).unwrap_err();
    assert!(
        migrated
            .to_string()
            .contains("incorrect mailbox vCPU0 status")
    );
}
