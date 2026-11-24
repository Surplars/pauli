//! Same owner and runnable target: distinguish reserved slots from timerless WFI peers.
//! Ownership: management creates private firmware clones; the remote dispatcher runs them.
//! Invariant: no peer re-enters VS during a scored batch. Only host Instant measures cost.
use super::*;
use crate::{monitor::backend::Target, tests::model::Timer};
use pauli_config::{CpuAffinity, FirmwareLayout};
use pauli_control::{Backend, ChildName, VmId};
use std::{format, time::Instant, vec::Vec};

const TARGET: VmId = VmId(2);
const REMOTE: usize = 1;
// (Allocated clones, started and parked). The first row isolates directory growth;
// the last three keep IDs, placement groups and backing allocations identical in shape.
const SHAPES: [(usize, usize); 4] = [(0, 0), (8, 0), (8, 4), (8, 8)];

struct Case {
    f: Fixture,
    owner: Dispatcher<Timer>,
    peers: Vec<usize>,
    parked: usize,
}

impl Case {
    fn new((allocated, parked): (usize, usize)) -> Self {
        let f = Fixture::with_config(2, [1, 1], 4, |slot, vm| {
            vm.memory_size = 32 * 1024;
            vm.firmware = Some(FirmwareLayout {
                rx_pages: 1,
                ro_pages: 0,
            });
            if slot == 1 {
                vm.cpu_affinity = CpuAffinity::Workers;
            }
        });
        let rt = &f.runtime;
        let mut host = Target(rt);
        host.start(VmId(1)).unwrap(); // The control belongs to the boot owner.
        host.start(TARGET).unwrap();
        let mut peers = Vec::new();
        for i in 0..allocated {
            let spec = ChildName::new(&format!("park{i}")).unwrap().into();
            let id = host.create(None, TARGET, spec).unwrap().vm;
            let slot = id.0 as usize - 1;
            let vm = rt.manager.vm(slot).unwrap();
            let cpu = vm.cpu(0).unwrap();
            assert_eq!(cpu.owner, REMOTE);
            if i < parked {
                host.start(id).unwrap();
                cpu.data
                    .lock()
                    .context
                    .as_mut()
                    .unwrap()
                    .script
                    .push_back(ExitKind::Wait);
            }
            peers.push(slot);
        }
        OWNER.with(|o| o.set(REMOTE));
        let mut owner = Dispatcher::new();
        // Let every started clone execute WFI, then give the sole runnable target a slice
        // so every topology starts the timed loop at the same round-robin position.
        for _ in 0..parked + 2 {
            assert!(rt.dispatch(&mut owner).unwrap());
        }
        let case = Self {
            f,
            owner,
            peers,
            parked,
        };
        assert_eq!(case.target_steps(), 6);
        case.parked_reads();
        case
    }

    fn target_steps(&self) -> u64 {
        let vm = self.f.runtime.manager.vm(TARGET.0 as usize - 1).unwrap();
        let cpu = vm.cpu(0).unwrap();
        assert_eq!((cpu.owner, cpu.run_state()), (REMOTE, CpuState::Runnable));
        cpu.data.lock().context.as_ref().unwrap().steps
    }

    fn parked_reads(&self) -> usize {
        self.peers
            .iter()
            .enumerate()
            .map(|(i, &slot)| {
                let vm = self.f.runtime.manager.vm(slot).unwrap();
                let cpu = vm.cpu(0).unwrap();
                if i >= self.parked {
                    assert_eq!(vm.lifecycle.state(), State::Stopped);
                    assert!(cpu.retired());
                    assert_eq!(cpu.data.lock().context.as_ref().unwrap().steps, 0);
                    assert_eq!(counts(&vm), (0, 0, 0));
                    return 0;
                }
                assert_eq!(vm.lifecycle.state(), State::Ready);
                assert_eq!((cpu.owner, cpu.run_state()), (REMOTE, CpuState::Waiting));
                let data = cpu.data.lock();
                let context = data.context.as_ref().unwrap();
                assert_eq!((context.steps, context.deadline), (1, None));
                drop(data);
                counts(&vm).0
            })
            .sum()
    }

    fn batch(&mut self, slices: usize) -> (u128, usize) {
        let before = (self.target_steps(), self.parked_reads());
        let started = Instant::now();
        for _ in 0..slices {
            assert!(self.f.runtime.dispatch(&mut self.owner).unwrap());
        }
        let ns = started.elapsed().as_nanos();
        assert_eq!(self.target_steps() - before.0, 3 * slices as u64);
        (ns, self.parked_reads() - before.1)
    }
}

#[test]
fn runnable_target_keeps_progress_with_real_parked_peers() {
    for n in SHAPES {
        let mut case = Case::new(n);
        case.batch(4); // Do not require a future scheduler to rescan sleeping vCPUs.
    }
}

#[test]
fn parked_owner_acknowledges_remote_fence_without_guest_entry() {
    use crate::arch::{Control, ControlReply};
    let f = Fixture::new(2, 2);
    let rt = &f.runtime;
    rt.manager.start(rt.board, 0).unwrap();
    let vm = rt.manager.vm(0).unwrap();
    let target = vm.cpu(0).unwrap();
    let sender = vm.cpu(1).unwrap();
    assert_eq!((target.owner, sender.owner), (0, 1));
    let mut target_dispatch = Dispatcher::new();
    let mut sender_dispatch = Dispatcher::new();
    let script = |slot: &crate::vm::slot::VcpuSlot<crate::tests::model_cpu::Cpu>, exit| {
        slot.data
            .lock()
            .context
            .as_mut()
            .unwrap()
            .script
            .push_back(exit);
    };
    script(target, ExitKind::Wait);
    assert!(rt.dispatch(&mut target_dispatch).unwrap());
    assert!(!rt.dispatch(&mut target_dispatch).unwrap());
    assert_eq!(target.run_state(), CpuState::Waiting);

    // RFENCE is host maintenance, not a guest interrupt. Only its sender should resume.
    let entry = (vm.config.guest_base + pauli_config::KERNEL_OFFSET) as usize;
    sender.request_start(entry, 0).unwrap();
    script(
        sender,
        ExitKind::Control(Control::Fence {
            mask: 1,
            base: 0,
            instruction: false,
        }),
    );
    OWNER.with(|o| o.set(sender.owner));
    assert!(rt.dispatch(&mut sender_dispatch).unwrap());
    assert_eq!(sender.run_state(), CpuState::Blocked);
    assert!(target.mailbox.lock().pending());
    assert_eq!(target.mailbox.lock().armed(), 1 << sender.index);
    assert!(!rt.dispatch(&mut sender_dispatch).unwrap()); // Cannot return before the ACK.
    #[cfg(feature = "selftest")]
    assert_eq!(target.mailbox.lock().ready_count.get(), 0);
    let (_, loads, puts) = counts(&vm);

    OWNER.with(|o| o.set(target.owner));
    assert!(!rt.dispatch(&mut target_dispatch).unwrap());
    assert!(!target.mailbox.lock().pending());
    assert!(!rt.dispatch(&mut target_dispatch).unwrap());
    let (_, new_loads, new_puts) = counts(&vm);
    assert_eq!((new_loads, new_puts), (loads, puts));
    {
        let data = target.data.lock();
        let cpu = data.context.as_ref().unwrap();
        assert_eq!((data.state, cpu.steps, cpu.ipis), (CpuState::Waiting, 1, 0));
    }
    OWNER.with(|o| o.set(sender.owner));
    assert!(rt.dispatch(&mut sender_dispatch).unwrap());
    let data = sender.data.lock();
    assert_eq!(data.state, CpuState::Runnable);
    assert_eq!(
        data.context.as_ref().unwrap().reply,
        Some(ControlReply::Success(0))
    );
    let inbox = target.mailbox.lock();
    assert_eq!((inbox.disarms.get(), inbox.armed()), (0, 0));
    #[cfg(feature = "selftest")]
    assert_eq!(
        (inbox.ready_count.get(), inbox.ready.get()),
        (1, Some((false, 1, sender.index, true)))
    );
}

#[test]
#[ignore = "host dispatch sample; run explicitly in release mode"]
fn sample_parked_peer_cost() {
    const SLICES: usize = 4096;
    let mut cases = SHAPES.map(Case::new);
    let mut times = [[0; 12]; SHAPES.len()];
    let mut reads = [0; SHAPES.len()];
    for round in 0..SHAPES.len() + times[0].len() {
        // Rotate order: in 12 scored rounds every shape takes each position three times.
        for offset in 0..SHAPES.len() {
            let i = (offset + round) % SHAPES.len();
            let (ns, inspected) = cases[i].batch(SLICES);
            if round >= SHAPES.len() {
                times[i][round - SHAPES.len()] = ns;
                reads[i] += inspected;
            }
        }
    }
    for (i, batches) in times.iter_mut().enumerate() {
        batches.sort_unstable();
        let ns = |j: usize| batches[j] as f64 / SLICES as f64;
        let reads_per_slice = reads[i] as f64 / (SLICES * batches.len()) as f64;
        std::println!(
            "clones={} waiting={} groups={} target_entries/slice=3 peer_entries=0 peer_irq_checks/slice={reads_per_slice:.2} host ns/dispatch p25={:.1} p50={:.1} p75={:.1}",
            SHAPES[i].0,
            SHAPES[i].1,
            cases[i].f.runtime.manager.placement_groups(),
            ns(2),
            ns(5),
            ns(8)
        );
    }
}
