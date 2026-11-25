//! Drive the boot barrier and VM lifecycle through their normal management entries.
//! Native firmware replies and the worker's online publication run on a model clock.
use super::model::{
    CLOCK_READ, Fixture, KICK_IN_FLIGHT, KICKS, NOW, OWNER, REJECT_KICK_ONCE, REJECT_NEXT_IRQ_CHIP,
    REJECT_WORKER_START, Smp, WORKER_STARTS,
};
use crate::{
    HvError,
    arch::{ArchSmp, SmpError},
    monitor::Monitor,
    runtime::{Dispatcher, WorkerStartupError},
    vm::{ActivationFault, KickFault, State},
};
use core::{cell::Cell, sync::atomic::Ordering};
use pauli_config::{CpuAffinity, FirmwareLayout, MAX_GUEST_VCPUS};
use std::{boxed::Box, rc::Rc};

fn fixture(host_cpus: usize, autostart: bool) -> Fixture {
    KICKS.with(|kicks| kicks.set(0));
    Fixture::with_config(host_cpus, [1, 1], 0, |slot, vm| {
        // Keep the retained runtime and RAM small enough for the boot tests.
        vm.memory_size = 32 * 1024;
        vm.firmware = Some(FirmwareLayout {
            rx_pages: 1,
            ro_pages: 0,
        });
        vm.autostart = autostart;
        let mut owners = [0; MAX_GUEST_VCPUS as usize];
        owners[0] = u8::from(slot == 0 && host_cpus > 1);
        vm.cpu_affinity = CpuAffinity::Physical(owners);
    })
}

struct ClockHook;
impl Drop for ClockHook {
    fn drop(&mut self) {
        CLOCK_READ.with(|slot| slot.take());
    }
}

fn on_clock(hook: impl FnMut() + 'static) -> ClockHook {
    CLOCK_READ.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    ClockHook
}

#[test]
fn autostart_waits_for_each_distinct_worker_to_publish_online() {
    let f = Box::leak(Box::new(fixture(3, true)));
    let rt = &f.runtime;
    let reads = Rc::new(Cell::new(0));
    let observed = reads.clone();
    let _clock = on_clock(move || {
        let read = observed.get();
        if read < 4 {
            assert_eq!(WORKER_STARTS.with(Cell::get), 0b110);
            assert!(rt.manager.vms().all(|vm| vm.lifecycle.joined()));
            assert_eq!(KICKS.with(Cell::get), 0);
            match read {
                1 | 2 => rt.publish_worker_online(1),
                3 => rt.publish_worker_online(2),
                _ => {}
            }
        }
        observed.set(read + 1);
        NOW.with(|n| n.set(n.get() + 1));
    });
    KICK_IN_FLIGHT.with(|slot| {
        slot.set(Some(Box::new(move |_| {
            assert!(
                reads.get() >= 4,
                "autostart sent a kick before every worker ACK"
            );
        })))
    });
    rt.start_workers().unwrap();
    assert!(rt.manager.vms().all(|vm| vm.lifecycle.can_enter()));
    assert_eq!(KICKS.with(Cell::get), 2);
}

#[test]
fn ready_timeout_names_only_the_absent_worker_and_starts_no_guest() {
    let f = Box::leak(Box::new(fixture(64, true)));
    let rt = &f.runtime;
    let mut reads = 0;
    // The one-second native TIME deadline crosses zero.
    NOW.with(|n| n.set(u64::MAX - 100));
    let _clock = on_clock(move || {
        if reads == 0 {
            assert_eq!(WORKER_STARTS.with(Cell::get), !1u64);
            for owner in 1..63 {
                rt.publish_worker_online(owner);
            }
        } else {
            NOW.with(|n| n.set(n.get().wrapping_add(500_000)));
        }
        reads += 1;
        assert!(reads <= 5, "boot ignored the elapsed native timeout");
    });
    assert_eq!(
        rt.start_workers(),
        Err(WorkerStartupError::ReadyTimeout {
            owner: 63,
            hart: Smp::hart_id(63),
            missing: 1 << 63,
        })
    );
    // Even an acknowledgement arriving after return cannot publish a VM.
    rt.publish_worker_online(63);
    assert!(rt.manager.vms().all(|vm| vm.lifecycle.joined()));
    assert_eq!(KICKS.with(Cell::get), 0);
}

#[test]
fn rejected_hsm_start_preserves_the_native_hart_and_firmware_error() {
    let f = Box::leak(Box::new(fixture(3, true)));
    let rt = &f.runtime;
    REJECT_WORKER_START.with(|owner| owner.set(Some(2)));
    assert_eq!(
        rt.start_workers(),
        Err(WorkerStartupError::HsmStart {
            owner: 2,
            failure: SmpError {
                error: HvError::Fault,
                hart: Smp::hart_id(2),
                raw: Some(-6)
            },
        })
    );
    assert_eq!(WORKER_STARTS.with(Cell::get), 0b110);
    assert!(rt.manager.vms().all(|vm| vm.lifecycle.joined()));
    assert_eq!(KICKS.with(Cell::get), 0);
}

#[test]
fn single_hart_boot_needs_no_worker_or_barrier_clock_read() {
    let f = Box::leak(Box::new(fixture(1, false)));
    let _clock = on_clock(|| panic!("single-hart boot sampled a worker deadline"));
    f.runtime.start_workers().unwrap();
    assert_eq!(WORKER_STARTS.with(Cell::get), 0);
}

fn rejected_kick(epoch: u32) -> KickFault {
    KickFault {
        epoch,
        owner: 1,
        source: SmpError {
            error: HvError::Fault,
            hart: Smp::hart_id(1),
            raw: Some(-4),
        },
    }
}

#[test]
fn autostart_kick_failure_names_its_vm_and_leaves_join_recoverable() {
    let f = Box::leak(Box::new(fixture(2, true)));
    let rt = &f.runtime;
    let _clock = on_clock(move || rt.publish_worker_online(1));
    REJECT_KICK_ONCE.with(|owner| owner.set(Some(1)));
    rt.start_workers().unwrap();
    let vm = rt.manager.vm(0).unwrap();
    let peer = rt.manager.vm(1).unwrap();
    let epoch = vm.epoch();
    assert_eq!(
        vm.faults().activation,
        Some(ActivationFault::StartKick(rejected_kick(epoch)))
    );
    assert_eq!(vm.faults().shutdown_kick, None);
    assert_eq!(vm.join_error(), None);
    assert!(!vm.lifecycle.can_enter());
    assert!(peer.lifecycle.can_enter());
    assert_eq!(peer.faults().activation, None);
    let mut monitor = Monitor::new();
    let text = super::uart_capture::host_output(|| monitor.poll(rt));
    let line = std::format!("VM 1 start-kick error=Fault epoch={epoch} owner=1 hart=66 sbi=-4");
    assert_eq!(text.lines().filter(|&s| s == line).count(), 1);
    assert!(!text.contains("I/O join error"));
    assert!(!super::uart_capture::host_output(|| monitor.poll(rt)).contains("error="));
    assert!(vm.try_join(0, rt.manager.link()).unwrap());
    assert_eq!(vm.lifecycle.state(), State::Faulted);
    rt.manager.start(rt.board, 0).unwrap();
    assert_ne!(vm.epoch(), epoch);
    assert!(vm.lifecycle.can_enter());
    assert_eq!(vm.faults().activation, None);
}

#[test]
fn failed_cold_activation_preserves_attempt_epoch_and_requires_cleanup_before_retry() {
    let f = fixture(2, false);
    let rt = &f.runtime;
    let vm = rt.manager.vm(0).unwrap();
    rt.manager.start(rt.board, 0).unwrap();
    let old_epoch = vm.epoch();
    vm.host_stop();
    assert!(vm.try_join(0, rt.manager.link()).unwrap());
    REJECT_NEXT_IRQ_CHIP.with(|reject| reject.set(true));
    assert_eq!(rt.manager.start(rt.board, 0), Err(HvError::Unsupported));
    let Some(ActivationFault::Activation {
        epoch,
        owner,
        error,
    }) = vm.faults().activation
    else {
        panic!("missing cold activation failure");
    };
    assert_eq!((owner, error), (0, HvError::Unsupported));
    assert!(epoch > old_epoch);
    assert_eq!(vm.epoch(), old_epoch);
    assert!(!vm.lifecycle.can_enter());
    assert!(!vm.lifecycle.joined());
    assert_eq!(vm.join_error(), None);
    assert_eq!(rt.manager.start(rt.board, 0), Err(HvError::Busy));
    let mut monitor = Monitor::new();
    let text = super::uart_capture::host_output(|| monitor.poll(rt));
    let line = std::format!("VM 1 activation error=Unsupported epoch={epoch} owner=0");
    assert!(text.lines().any(|s| s == line));
    assert!(!text.contains("I/O join error"));
    assert!(vm.lifecycle.joined());
    rt.manager.start(rt.board, 0).unwrap();
    assert!(vm.epoch() > epoch && vm.lifecycle.can_enter());
}

#[test]
fn shutdown_kick_failure_retains_retry_until_the_owner_retires() {
    let f = fixture(2, false);
    let rt = &f.runtime;
    rt.manager.start(rt.board, 0).unwrap();
    let vm = rt.manager.vm(0).unwrap();
    let mut worker = Dispatcher::new();
    OWNER.with(|owner| owner.set(1));
    assert!(rt.dispatch(&mut worker).unwrap());
    assert!(!vm.retired());
    OWNER.with(|owner| owner.set(0));
    REJECT_KICK_ONCE.with(|owner| owner.set(Some(1)));
    vm.host_stop();
    let failure = rejected_kick(vm.epoch());
    assert_eq!(vm.faults().shutdown_kick, Some(failure));
    assert_eq!(vm.faults().activation, None);
    assert_eq!(vm.join_error(), None);
    assert_eq!(vm.devices.wake.load(Ordering::Acquire), 1);
    assert!(!vm.lifecycle.can_enter());
    assert!(!vm.try_join(0, rt.manager.link()).unwrap());
    vm.deliver_wakes();
    assert_eq!(vm.devices.wake.load(Ordering::Acquire), 0);
    assert!(!vm.try_join(0, rt.manager.link()).unwrap());
    OWNER.with(|owner| owner.set(1));
    rt.dispatch(&mut worker).unwrap();
    OWNER.with(|owner| owner.set(0));
    assert!(vm.retired());
    assert!(vm.try_join(0, rt.manager.link()).unwrap());
    assert_eq!(vm.lifecycle.state(), State::Stopped);
    assert_eq!(vm.faults().shutdown_kick, Some(failure));
}
