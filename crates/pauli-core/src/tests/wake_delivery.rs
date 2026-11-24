//! Native IPI fanout after the IRQ chip has published a VM-local lane mask.
//! The model records the actual physical endpoints and can reject one delivery.
use super::model::{Fixture, KICKED_OWNERS, KICKS, OWNER, REJECT_KICK_ONCE};
use crate::{arch::MAX_VCPUS, runtime::Dispatcher};
use core::sync::atomic::Ordering;

const LAST: usize = MAX_VCPUS - 1;
const SHARED: usize = 1usize << 63;
const HIGH: usize = 1usize << 32;

fn kicks() -> (u64, usize) {
    (
        KICKS.with(|k| k.replace(0)),
        KICKED_OWNERS.with(|k| k.replace(0)),
    )
}

fn tenants() -> Fixture {
    let f = Fixture::with_config(64, [MAX_VCPUS as u8, 1], 0, |id, vm| {
        let mut pins = [0; MAX_VCPUS];
        if id == 0 {
            pins[1..3].fill(63);
            pins[LAST] = 32;
        } else {
            pins[0] = 7;
        }
        vm.cpu_affinity = pauli_config::CpuAffinity::Physical(pins);
    });
    f.runtime.manager.start(f.runtime.board, 0).unwrap();
    f.runtime.manager.start(f.runtime.board, 1).unwrap();
    kicks();
    f
}

#[test]
fn targeted_delivery_coalesces_physical_owners_and_retries_only_failures() {
    let f = tenants();
    let vm = f.runtime.manager.vm(0).unwrap();
    let lanes = (1 << 1) | (1 << 2) | (1 << LAST);
    vm.devices.wake.fetch_or(lanes, Ordering::Release);
    REJECT_KICK_ONCE.with(|k| k.set(Some(63)));
    vm.deliver_wakes();
    assert_eq!(kicks(), (2, SHARED | HIGH));

    // A new lane arrives before retry. Do not lose it, repeat the successful
    // high lane, or send two retries to the shared owner.
    vm.devices.wake.fetch_or(1, Ordering::Release);
    vm.deliver_wakes();
    assert_eq!(kicks(), (2, SHARED | 1));
    vm.deliver_wakes();
    assert_eq!(kicks(), (0, 0));
}

#[test]
fn full_stop_retires_before_restart_and_a_new_epoch_discards_old_wakes() {
    let f = tenants();
    let rt = &f.runtime;
    let vm = rt.manager.vm(0).unwrap();
    let peer = rt.manager.vm(1).unwrap();
    let peer_epoch = peer.epoch();
    let mut local = Dispatcher::new();
    OWNER.with(|o| o.set(0));
    assert!(rt.dispatch(&mut local).unwrap()); // Retire a real model entry, not an untouched boot slot.
    let old = vm.epoch();
    vm.devices.wake.fetch_or(1 << LAST, Ordering::Release);
    vm.host_stop();
    assert_eq!(kicks(), (3, SHARED | HIGH | 1));
    assert!(!vm.try_join(0, rt.manager.link()).unwrap());
    rt.dispatch(&mut local).unwrap();
    assert!(vm.try_join(0, rt.manager.link()).unwrap());
    rt.manager.start(rt.board, 0).unwrap();
    assert_ne!(vm.epoch(), old);
    assert_eq!(kicks(), (3, SHARED | HIGH | 1));
    vm.deliver_wakes();
    assert_eq!(kicks(), (0, 0));
    assert_eq!(peer.epoch(), peer_epoch);
}
