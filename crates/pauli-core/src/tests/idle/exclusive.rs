//! A Linux domain owns the boot hart, a shared worker, and a native disk.
//! Its peer's exclusive lanes take real dispatch, WFI and management paths.
use super::{Dispatcher, ExitKind, Fixture, IDLE_DEADLINE, IDLE_ENTRIES, NOW, Timer, idle, park};
use crate::{
    arch::{Control, ControlError, ControlReply},
    board::BootImages,
    tests::{
        model::{IDLE_KICK, OWNER, REJECT_KICK_ONCE},
        storage::Native,
    },
    vm::slot::CpuState,
};
use pauli_config::{CpuAffinity, DiskConfig, FirmwareLayout, MAX_GUEST_VCPUS};

fn fixture(vcpus: u8, firmware: bool) -> Fixture {
    let disk = DiskConfig {
        sectors: 64,
        writable: true,
        managed: false,
    };
    let f = Fixture::with_disks(
        4,
        [2, vcpus],
        0,
        [Some((disk, Native::new())), None, None, None],
        |slot, vm| {
            let mut pins = [0; MAX_GUEST_VCPUS as usize];
            if slot == 0 {
                pins[..2].copy_from_slice(&[0, 3]);
                vm.disk = Some(0);
            } else {
                vm.exclusive_cpus = true;
                pins[..usize::from(vcpus)].copy_from_slice(&[2, 1][..usize::from(vcpus)]);
                if firmware {
                    vm.memory_size = 32 * 1024;
                    vm.firmware = Some(FirmwareLayout {
                        rx_pages: 1,
                        ro_pages: 0,
                    });
                }
            }
            if !firmware {
                vm.net_peer = Some((1 - slot) as u8);
            }
            vm.cpu_affinity = CpuAffinity::Physical(pins);
            BootImages {
                kernel: &[0x31; 64],
                initrd: &[],
                dtb: &[0xd1; 64],
            }
        },
    );
    for slot in 0..2 {
        f.runtime.manager.start(f.runtime.board, slot).unwrap();
    }
    f
}

fn disarmed(f: &Fixture, local: &mut Dispatcher<Timer>) {
    IDLE_ENTRIES.with(|n| n.set(0));
    f.runtime.idle(local).unwrap();
    assert_eq!(
        IDLE_ENTRIES.with(|n| n.get()),
        1,
        "owner went through native WFI"
    );
    assert_eq!(
        IDLE_DEADLINE.with(|d| d.get()),
        None,
        "physical compare disarmed"
    );
}

#[test]
fn exclusive_waiter_uses_its_own_timer_on_a_mixed_host() {
    for firmware in [true, false] {
        let f = fixture(1, firmware);
        let rt = &f.runtime;
        let vm = rt.manager.vm(1).unwrap();
        let cpu = vm.cpu(0).unwrap();
        let mut local = Dispatcher::new();
        park(&f, 1, &mut local);
        NOW.with(|n| n.set(1000));
        disarmed(&f, &mut local);
        cpu.data.lock().context.as_mut().unwrap().deadline = Some(9000);
        assert_eq!(idle(&f, &mut local), Some(9000));
        for owner in [0, 3] {
            OWNER.with(|o| o.set(owner));
            assert_eq!(idle(&f, &mut local), Some(1100), "shared owner {owner}");
        }
        OWNER.with(|o| o.set(cpu.owner));
        NOW.with(|n| n.set(9000));
        assert_eq!(idle(&f, &mut local), None);
        assert!(rt.dispatch(&mut local).unwrap());
        vm.host_stop();
        assert_eq!(idle(&f, &mut local), Some(NOW.with(|n| n.get()) + 100));
        assert!(!rt.dispatch(&mut local).unwrap());
        OWNER.with(|o| o.set(0));
        rt.manager.service(rt.board);
        assert!(vm.lifecycle.joined());
        OWNER.with(|o| o.set(cpu.owner));
        disarmed(&f, &mut local);
    }
}

#[test]
fn rtc_serviced_by_management_wakes_a_disarmed_exclusive_worker() {
    let f = fixture(1, true);
    let rt = &f.runtime;
    let mut local = Dispatcher::new();
    park(&f, 1, &mut local);
    assert!(!rt.dispatch(&mut local).unwrap());
    NOW.with(|n| n.set(1000));
    let rtc = pauli_config::GuestBoardLayout::DEFAULT.rtc.base;
    super::rtc::write(&f, 1, rtc + 0x10, 1); // IRQ enable
    super::rtc::write(&f, 1, rtc + 0x08, 1_100_000); // alarm in ns
    disarmed(&f, &mut local);
    assert!(!IDLE_KICK.with(|k| k.get()));

    OWNER.with(|o| o.set(0));
    NOW.with(|n| n.set(1100));
    rt.manager.service(rt.board);
    OWNER.with(|o| o.set(2));
    // The event was published after the worker's empty dispatch; the native
    // level survives disarming the physical compare and is observed at WFI.
    disarmed(&f, &mut local);
    assert!(IDLE_KICK.with(|k| k.get()));
    assert!(rt.dispatch(&mut local).unwrap());
}

#[test]
fn exclusive_secondary_wakes_for_published_hsm_and_ipi_after_native_kick_retry() {
    let f = fixture(2, false);
    let rt = &f.runtime;
    let vm = rt.manager.vm(1).unwrap();
    let (sender, target) = (vm.cpu(0).unwrap(), vm.cpu(1).unwrap());
    assert_eq!((sender.owner, target.owner), (2, 1));
    let (mut primary, mut secondary) = (Dispatcher::new(), Dispatcher::new());
    OWNER.with(|o| o.set(target.owner));
    assert!(!rt.dispatch(&mut secondary).unwrap()); // ACK any boot IPI.
    disarmed(&f, &mut secondary);
    assert!(!IDLE_KICK.with(|k| k.get()));

    for action in [
        Control::Start {
            cpu: 1,
            entry: vm.config.guest_base as usize,
            opaque: 0,
        },
        Control::Ipi { mask: 1, base: 1 },
    ] {
        let starting = matches!(action, Control::Start { .. });
        OWNER.with(|o| o.set(sender.owner));
        sender
            .data
            .lock()
            .context
            .as_mut()
            .unwrap()
            .script
            .push_back(ExitKind::Control(action));
        REJECT_KICK_ONCE.with(|reject| reject.set(Some(target.owner)));
        assert!(rt.dispatch(&mut primary).unwrap());
        assert_eq!(
            sender.data.lock().context.as_ref().unwrap().reply,
            Some(ControlReply::Error(ControlError::Failed))
        );
        OWNER.with(|o| o.set(0));
        rt.manager.service(rt.board);
        OWNER.with(|o| o.set(target.owner));
        rt.idle(&mut secondary).unwrap();
        assert!(
            IDLE_KICK.with(|k| k.get()),
            "management retried the exact target owner"
        );

        if starting {
            target
                .data
                .lock()
                .context
                .as_mut()
                .unwrap()
                .script
                .push_back(ExitKind::Wait);
        }
        assert!(rt.dispatch(&mut secondary).unwrap());
        if starting {
            assert_eq!(target.run_state(), CpuState::Waiting);
            target.data.lock().context.as_mut().unwrap().deadline = Some(50_000);
            assert_eq!(
                idle(&f, &mut secondary),
                Some(50_000),
                "uses the nonzero vCPU's compare"
            );
            target.data.lock().context.as_mut().unwrap().deadline = None;
            disarmed(&f, &mut secondary);
            assert!(
                !IDLE_KICK.with(|k| k.get()),
                "dispatcher consumed the HSM wake"
            );
        } else {
            assert_eq!(target.data.lock().context.as_ref().unwrap().ipis, 1);
        }
    }
    #[cfg(feature = "selftest")]
    {
        assert_eq!(rt.worker_wait(0), None);
        assert_eq!(rt.worker_wait(3), None);
        assert_eq!(rt.worker_wait(target.owner), Some((false, 1)));
    }
}
