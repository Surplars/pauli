//! Unified production dispatcher tests with explicit ISA/board boundary fixtures.
//! Ownership/Lock Ordering: simulate each pinned CPU sequentially; atomics tested separately.
//! Invariants: fault/stop freeze only one VM; restart follows every owner's retirement.
use super::model::{Fixture, OWNER, Timer};
use crate::{
    arch::{Control, ExitKind},
    runtime::Dispatcher,
    trace::Tracer,
    vm::{State, slot::CpuState},
};
fn steps(f: &Fixture, slot: usize, cpu: usize) -> u64 {
    f.runtime
        .manager
        .vm(slot)
        .unwrap()
        .cpu(cpu)
        .unwrap()
        .data
        .lock()
        .context
        .as_ref()
        .unwrap()
        .steps
}
#[test]
fn single_cpu_overcommit_and_smp_share_one_lifecycle() {
    for host_cpus in [1, 2] {
        let f = Fixture::new(host_cpus, 2);
        let rt = &f.runtime;
        rt.manager.start(rt.board, 0).unwrap();
        rt.manager.start(rt.board, 1).unwrap();
        let mut owners = [Dispatcher::new(), Dispatcher::new()];
        for _ in 0..4 {
            for (owner, dispatcher) in owners.iter_mut().enumerate().take(host_cpus) {
                OWNER.with(|o| o.set(owner));
                rt.dispatch(dispatcher).unwrap();
            }
        }
        assert!(steps(&f, 0, 0) > 0 && steps(&f, 1, 0) > 0);
        let vm = rt.manager.vm(0).unwrap();
        vm.cpu(1)
            .unwrap()
            .request_start(
                vm.config.guest_base as usize + pauli_config::KERNEL_OFFSET as usize,
                0,
            )
            .unwrap();
        for _ in 0..8 {
            for (owner, dispatcher) in owners.iter_mut().enumerate().take(host_cpus) {
                OWNER.with(|o| o.set(owner));
                rt.dispatch(dispatcher).unwrap();
            }
        }
        assert!(steps(&f, 0, 1) > 0);
        let old_epoch = vm.epoch();
        vm.request_stop(true, true);
        assert!(!vm.try_join(0, rt.manager.link()).unwrap());
        let before = steps(&f, 1, 0);
        for (owner, dispatcher) in owners.iter_mut().enumerate().take(host_cpus) {
            OWNER.with(|o| o.set(owner));
            rt.dispatch(dispatcher).unwrap();
        }
        assert!(vm.try_join(0, rt.manager.link()).unwrap());
        assert_eq!(vm.lifecycle.state(), State::Faulted);
        assert!(steps(&f, 1, 0) > before || host_cpus == 2);
        rt.manager.start(rt.board, 0).unwrap();
        assert_ne!(vm.epoch(), old_epoch);
        assert_eq!(steps(&f, 0, 0), 0);
        assert_eq!(vm.cpu(1).unwrap().data.lock().state, CpuState::Stopped);
        for guest in rt.manager.vms() {
            guest.host_stop();
        }
        for (owner, dispatcher) in owners.iter_mut().enumerate().take(host_cpus) {
            OWNER.with(|o| o.set(owner));
            rt.dispatch(dispatcher).unwrap();
        }
        for guest in rt.manager.vms() {
            assert!(guest.try_join(0, rt.manager.link()).unwrap());
        }
    }
}
#[test]
fn deferred_fence_and_hsm_stop_do_not_renew_budget() {
    // Only a different native owner needs a later dispatch to fulfill its ticket.
    let f = Fixture::new(2, 2);
    let rt = &f.runtime;
    rt.manager.start(rt.board, 0).unwrap();
    let vm = rt.manager.vm(0).unwrap();
    vm.cpu(1)
        .unwrap()
        .request_start(
            vm.config.guest_base as usize + pauli_config::KERNEL_OFFSET as usize,
            0,
        )
        .unwrap();
    vm.cpu(0)
        .unwrap()
        .data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Control(Control::Fence {
            mask: 3,
            base: 0,
            instruction: false,
        }));
    let mut owner = Dispatcher::new();
    let mut peer = Dispatcher::new();
    rt.dispatch(&mut owner).unwrap();
    assert_eq!(vm.cpu(0).unwrap().data.lock().state, CpuState::Blocked);
    OWNER.with(|o| o.set(1));
    rt.dispatch(&mut peer).unwrap();
    OWNER.with(|o| o.set(0));
    rt.dispatch(&mut owner).unwrap();
    assert_eq!(vm.cpu(0).unwrap().data.lock().state, CpuState::Runnable);
    assert!(steps(&f, 0, 0) <= 4); // One 300-tick absolute slice, not per exit.
    vm.cpu(1)
        .unwrap()
        .data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Control(Control::StopCpu));
    OWNER.with(|o| o.set(1));
    rt.dispatch(&mut peer).unwrap();
    assert_eq!(vm.cpu(1).unwrap().data.lock().state, CpuState::Stopped);
    assert!(vm.cpu(1).unwrap().retired());
    vm.host_stop();
    OWNER.with(|o| o.set(0));
    rt.dispatch(&mut owner).unwrap();
    assert!(vm.try_join(0, rt.manager.link()).unwrap());
}
#[test]
fn a_stream_of_same_hart_fences_does_not_buy_a_new_slice() {
    use crate::arch::ControlReply;
    let f = Fixture::new(1, 2);
    let rt = &f.runtime;
    rt.manager.start(rt.board, 0).unwrap();
    let vm = rt.manager.vm(0).unwrap();
    let peer = vm.cpu(1).unwrap();
    peer.request_start(
        (vm.config.guest_base + pauli_config::KERNEL_OFFSET) as usize,
        0,
    )
    .unwrap();
    let sender = vm.cpu(0).unwrap();
    sender.data.lock().context.as_mut().unwrap().script.extend(
        [ExitKind::Control(Control::Fence {
            mask: 3,
            base: 0,
            instruction: true,
        }); 5],
    );
    let mut owner = Dispatcher::new();
    rt.dispatch(&mut owner).unwrap();
    {
        let data = sender.data.lock();
        let cpu = data.context.as_ref().unwrap();
        assert_eq!(
            (data.state, cpu.reply),
            (CpuState::Runnable, Some(ControlReply::Success(0)))
        );
        assert_eq!((cpu.steps, cpu.script.len()), (3, 2)); // Original 300-tick slice.
    }
    assert_eq!(steps(&f, 0, 1), 0); // Synchronizing on its behalf isn't a guest entry.
    rt.dispatch(&mut owner).unwrap();
    assert_eq!(steps(&f, 0, 1), 3); // The ready sibling gets its turn next.
}
#[test]
fn owner_local_trace_covers_entries_exits_and_decoded_mmio() {
    #[derive(Default)]
    struct Trace {
        enters: usize,
        exits: usize,
        last_mmio: Option<(usize, bool)>,
    }
    impl Tracer for Trace {
        const MEASURE_ELAPSED: bool = true;
        fn on_vm_enter(&mut self, cpu: u8) {
            assert_eq!(cpu, 0);
            self.enters += 1;
        }
        fn on_vm_exit(&mut self, cpu: u8, kind: ExitKind, elapsed: u64) {
            assert_eq!((cpu, kind, elapsed), (0, ExitKind::Resume, 100));
            self.exits += 1;
        }
        fn on_mmio(&mut self, gpa: usize, write: bool) {
            // Elapsed guest time stays 100 ticks even when HS emulation takes time.
            super::model::NOW.with(|now| now.set(now.get() + 17));
            self.last_mmio = Some((gpa, write));
        }
    }
    assert_eq!(core::mem::size_of::<crate::trace::NoopTracer>(), 0);
    // One cursor and one bounded timer offer per physical owner, never per guest.
    assert!(core::mem::size_of::<Dispatcher<Timer>>() <= 40);
    let f = Fixture::new(1, 1);
    let rt = &f.runtime;
    rt.manager.start(rt.board, 0).unwrap();
    let gpa = pauli_config::GuestBoardLayout::DEFAULT.uart.base as usize;
    rt.manager
        .vm(0)
        .unwrap()
        .cpu(0)
        .unwrap()
        .data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .mmio = Some(pauli_hal::device::MmioAccess {
        gpa,
        width: 1,
        write: None,
    });
    let mut local = Dispatcher::with_tracer(Trace::default());
    assert!(rt.dispatch(&mut local).unwrap());
    let trace = local.tracer();
    assert_eq!((trace.enters, trace.exits), (3, 3));
    assert_eq!(trace.last_mmio, Some((gpa, false)));
}

#[test]
fn stats_tracer_records_slices_entries_hs_idle_ticks_and_exit_causes() {
    use super::model::NOW;
    use crate::trace::{HS_STATS_MAGIC, HS_STATS_VERSION, HsStats, StatsTracer};
    use core::sync::atomic::Ordering;

    static STATS: HsStats = HsStats::new();
    assert_eq!(STATS.magic, HS_STATS_MAGIC);
    assert_eq!(STATS.version, HS_STATS_VERSION);

    let f = Fixture::new(1, 1);
    let rt = &f.runtime;
    rt.manager.start(rt.board, 0).unwrap();
    let vm = rt.manager.vm(0).unwrap();
    let cpu = vm.cpu(0).unwrap();
    {
        let mut data = cpu.data.lock();
        let ctx = data.context.as_mut().unwrap();
        ctx.script.push_back(ExitKind::Resume);
        ctx.script.push_back(ExitKind::Wait);
    }

    NOW.with(|n| n.set(1_000));
    let mut local = Dispatcher::with_tracer(StatsTracer::with_stats(&STATS));
    assert!(rt.dispatch(&mut local).unwrap());
    // Simulated CPU executed 2 steps (each +100 ticks in model_cpu::Cpu::run):
    // step 1: Resume (1000 -> 1100), step 2: Wait (1100 -> 1200).
    assert_eq!(STATS.slices.load(Ordering::Relaxed), 1);
    assert_eq!(STATS.entries.load(Ordering::Relaxed), 2);
    assert_eq!(STATS.guest_ticks.load(Ordering::Relaxed), 200);
    assert_eq!(STATS.hs_ticks.load(Ordering::Relaxed), 0);
    assert_eq!(STATS.exit_wfi.load(Ordering::Relaxed), 1);
    assert_eq!(STATS.exit_other.load(Ordering::Relaxed), 1);
    assert_eq!(STATS.last_tick.load(Ordering::Relaxed), 1_200);

    // Retain the 50 HS ticks between the last guest exit and WFI.
    NOW.with(|n| n.set(1_250));
    rt.idle(&mut local).unwrap();
    assert_eq!(STATS.hs_ticks.load(Ordering::Relaxed), 50);
    assert_eq!(STATS.idle_ticks.load(Ordering::Relaxed), 0);

    use crate::arch::{ExitSource, TickInterval};
    static SEQUENCE: HsStats = HsStats::new();
    let mut trace = StatsTracer::with_stats(&SEQUENCE);
    trace.on_guest_interval(
        0,
        ExitSource::Other,
        TickInterval {
            start: 1_000,
            end: 1_200,
        },
    );
    trace.on_idle_end(1_300, 50);
    trace.on_idle_end(1_500, 150);
    trace.on_guest_interval(
        0,
        ExitSource::Wfi,
        TickInterval {
            start: 1_575,
            end: 1_675,
        },
    );
    assert_eq!(SEQUENCE.hs_ticks.load(Ordering::Relaxed), 175);
    assert_eq!(SEQUENCE.guest_ticks.load(Ordering::Relaxed), 300);
    assert_eq!(SEQUENCE.idle_ticks.load(Ordering::Relaxed), 200);
    assert_eq!(SEQUENCE.entries.load(Ordering::Relaxed), 2);
    assert_eq!(SEQUENCE.last_tick.load(Ordering::Relaxed), 1_675);
}
