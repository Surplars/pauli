//! Pair a byte-caused IRQ with the first owner sample, not with an arbitrary kick.
//! Ownership: each fixture drives both pinned harts in a known order.
//! Invariant: a chip sample without an unambiguous RX edge never reads the observation clock.
use super::*;

fn no_clock() -> u64 {
    panic!("an unpaired chip sample must not read TIME")
}

#[test]
fn unrelated_or_ambiguous_pending_does_not_sample_time() {
    let f = pair();
    let vm = f.runtime.manager.vm(0).unwrap();
    let pending = || {
        vm.devices
            .irqs
            .lock()
            .owner_pending::<crate::tests::model::Smp>(0, 1, no_clock)
    };
    reg(&f, IER, Some(1));
    assert!(!pending());
    vm.devices.irqs.lock().lines[0] = true; // Another source was already deliverable.
    assert!(vm.devices.receive_console(b'a', || 10));
    assert!(pending());
    #[cfg(feature = "selftest")]
    assert!(vm.devices.irqs.lock().rx.last.unwrap().owner.is_none());

    // Consume the next edge before the owner looks. A later pending line
    // cannot stand in for the original byte, even in the same VM epoch.
    assert_eq!(reg(&f, DATA, None), usize::from(b'a'));
    assert!(vm.devices.receive_console(b'b', || 20));
    assert_eq!(reg(&f, DATA, None), usize::from(b'b'));
    vm.devices.irqs.lock().lines[0] = true;
    assert!(pending());
    #[cfg(feature = "selftest")]
    assert!(vm.devices.irqs.lock().rx.last.unwrap().owner.is_none());
}

#[cfg(not(feature = "selftest"))]
#[test]
fn normal_owner_never_samples_an_observation_clock_even_for_a_real_edge() {
    let f = pair();
    let vm = f.runtime.manager.vm(0).unwrap();
    reg(&f, IER, Some(1));
    assert!(vm.devices.receive_console(b'p', || 20));
    assert!(
        vm.devices
            .irqs
            .lock()
            .owner_pending::<crate::tests::model::Smp>(0, 1, no_clock)
    );
}

#[cfg(feature = "selftest")]
#[test]
fn non_probe_guests_keep_the_rx_timestamp_but_not_an_owner_pair() {
    // A single-vCPU AIA firmware and a multi-vCPU PLIC Linux-shaped guest both
    // publish RX edges; neither satisfies the timing probe's delivery contract.
    let f = Fixture::with_config(2, [1, 2], 0, |slot, vm| {
        if slot == 0 {
            vm.firmware = Some(pauli_config::FirmwareLayout {
                rx_pages: 1,
                ro_pages: 0,
            });
            vm.memory_size = 32 * 1024;
            vm.aia = true;
        }
    });
    for slot in [0, 1] {
        f.runtime.manager.start(f.runtime.board, slot).unwrap();
        let vm = f.runtime.manager.vm(slot).unwrap();
        vm.devices.io.lock().console.access(IER, Some(1)).unwrap();
        assert!(vm.devices.receive_console(b'p', || 10));
        let mut bank = vm.devices.irqs.lock();
        assert!(bank.owner_pending::<crate::tests::model::Smp>(
            0,
            vm.cpu(0).unwrap().owner,
            no_clock
        ));
        assert!(bank.rx.last.unwrap().owner.is_none());
    }
}

#[cfg(feature = "selftest")]
#[test]
fn runnable_entry_pairs_once_and_does_not_consume_another_vms_edge() {
    let f = pair();
    let rt = &f.runtime;
    reg(&f, IER, Some(1));
    let vm = rt.manager.vm(0).unwrap();
    let peer = rt.manager.vm(1).unwrap();
    assert!(vm.devices.receive_console(b'p', || 10));
    assert!(
        !peer
            .devices
            .irqs
            .lock()
            .owner_pending::<crate::tests::model::Smp>(0, 1, no_clock)
    );
    assert!(peer.devices.irqs.lock().rx.last.is_none());
    assert!(vm.devices.irqs.lock().rx.last.unwrap().owner.is_none());
    OWNER.with(|o| o.set(1));
    NOW.with(|n| n.set(20));
    let mut local = Dispatcher::new();
    assert!(rt.dispatch(&mut local).unwrap());
    let sample = || {
        let owner = vm.devices.irqs.lock().rx.last.unwrap().owner.unwrap();
        (owner.vcpu, owner.owner, owner.ticks)
    };
    assert_eq!(sample(), (0, 1, 20));
    assert!(
        vm.devices
            .irqs
            .lock()
            .owner_pending::<crate::tests::model::Smp>(0, 1, no_clock)
    );
    NOW.with(|n| n.set(500));
    assert!(rt.dispatch(&mut local).unwrap());
    assert_eq!(sample(), (0, 1, 20)); // Repeated entry cannot replace the earliest timestamp.
}

#[cfg(feature = "selftest")]
#[test]
fn waiting_mail_scan_pairs_even_when_a_peer_enters_the_guest_first() {
    use crate::{
        arch::{ArchSmp, Mailbox},
        tests::model::Smp,
    };
    let f = pair();
    let rt = &f.runtime;
    let vm = rt.manager.vm(0).unwrap();
    let slot = vm.cpu(0).unwrap();
    rt.manager.start(rt.board, 1).unwrap();
    reg(&f, IER, Some(1));
    slot.data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Wait);
    OWNER.with(|o| o.set(1));
    let mut local = Dispatcher::new();
    assert!(rt.dispatch(&mut local).unwrap()); // Original sleeps; peer is next in round-robin.
    let steps = || slot.data.lock().context.as_ref().unwrap().steps;
    let before = steps();
    OWNER.with(|o| o.set(0));
    assert!(vm.devices.receive_console(b'p', || 200));
    slot.mailbox.lock().post_fence(false, 0);
    Smp::kick(slot.owner).unwrap();
    OWNER.with(|o| o.set(1));
    NOW.with(|n| n.set(250));
    assert!(rt.dispatch(&mut local).unwrap());
    assert_eq!(steps(), before); // Mailbox-only scan saw the IRQ without a guest entry.
    assert_eq!(
        vm.devices.irqs.lock().rx.last.unwrap().owner.unwrap().ticks,
        250
    );
    assert!(rt.dispatch(&mut local).unwrap());
    assert!(steps() > before);
    assert_eq!(
        vm.devices.irqs.lock().rx.last.unwrap().owner.unwrap().ticks,
        250
    );
}
