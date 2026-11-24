//! Owner-local selection across word boundaries and asymmetric guest topologies.
//! Ownership/Lock Ordering: each test owns its index or fixture, with no native workers.
//! Invariants: visit each assigned slot once per round; filtering one VM cannot alter a peer.
use super::model::Fixture;
use crate::{
    placement::{STRIDE, SlotSet},
    runtime::Dispatcher,
};

fn round<const WORDS: usize>(
    mut placed: SlotSet<WORDS>,
    mut cursor: usize,
) -> impl Iterator<Item = usize> {
    core::iter::from_fn(move || {
        let flat = placed.pop_from(cursor)?;
        cursor = (flat + 1) % (WORDS * usize::BITS as usize);
        Some(flat)
    })
}

#[test]
fn sparse_and_full_sets_wrap_once_at_every_cursor() {
    const WORD: usize = usize::BITS as usize;
    const LIMIT: usize = 3 * WORD;
    for enabled in [
        [false; LIMIT],
        [true; LIMIT],
        // Empty middle word; include both endpoints of the first and last words.
        core::array::from_fn(|i| i == 0 || i == WORD - 1 || i == 2 * WORD || i == LIMIT - 1),
    ] {
        let mut placed = SlotSet::<3>::EMPTY;
        for flat in (0..LIMIT).filter(|i| enabled[*i]) {
            placed.insert(flat / STRIDE, (flat % STRIDE) as u8);
        }
        for cursor in 0..LIMIT {
            let expected = (cursor..LIMIT).chain(0..cursor).filter(|i| enabled[*i]);
            assert!(round(placed, cursor).eq(expected), "cursor={cursor}");
        }
        let mut snapshot = placed;
        let mut groups = placed;
        let expected = enabled
            .chunks(STRIDE)
            .enumerate()
            .filter(|(_, lanes)| lanes.contains(&true))
            .map(|(id, _)| id);
        assert!(
            core::iter::from_fn(|| groups.pop_vm())
                .map(|(vm, lanes)| {
                    assert_eq!(lanes, placed.local_cpus(vm));
                    vm
                })
                .eq(expected)
        );
        let removed = WORD / STRIDE;
        snapshot.remove_vm(removed);
        let kept = (0..LIMIT).filter(|i| enabled[*i] && i / STRIDE != removed);
        assert!(round(snapshot, 0).eq(kept));
        assert!(round(placed, 0).eq((0..LIMIT).filter(|i| enabled[*i])));
        for (flat, enabled) in enabled.into_iter().enumerate() {
            let mask = placed.local_cpus(flat / STRIDE);
            assert_eq!(mask & (1 << (flat % STRIDE)) != 0, enabled);
        }
    }
}

#[test]
fn sparse_publication_shrinks_without_erasing_a_peer_or_changing_an_old_snapshot() {
    use crate::placement::PublishedSet;
    let per_word = usize::BITS as usize / STRIDE;
    let last = 2 * per_word;
    let p = PublishedSet::<3>::empty();
    p.set_vm(0, 1);
    p.set_vm(last, 1 << 15);
    p.set_vm(last + 1, 2);
    let old = p.snapshot();
    p.set_vm(last, 0);
    assert_eq!(p.snapshot().local_cpus(last + 1), 2);
    p.set_vm(last + 1, 0);
    let mut live = p.snapshot();
    assert_eq!(live.pop_vm(), Some((0, 1)));
    assert_eq!(live.pop_vm(), None);
    assert_eq!(old.local_cpus(last), 1 << 15);
    p.set_vm(last, 4);
    assert_eq!(p.snapshot().local_cpus(last), 4);
}

#[test]
fn a_hart_without_assigned_vcpus_leaves_peer_mail_pending() {
    use super::model::{OWNER, Smp};
    use crate::arch::{ArchSmp, Mailbox};
    let fixture = Fixture::new(3, 1);
    let rt = &fixture.runtime;
    rt.manager.start(rt.board, 0).unwrap();
    let boot = rt.manager.vm(0).unwrap();
    let cpu = boot.cpu(0).unwrap();
    cpu.mailbox.lock().post_ipi();
    Smp::kick(cpu.owner).unwrap();
    OWNER.with(|o| o.set(2)); // Both configured VM boot vCPUs belong to other harts.
    assert!(rt.manager.placed_on(0, 2).is_empty());
    assert!(!rt.dispatch(&mut Dispatcher::new()).unwrap());
    assert!(cpu.mailbox.lock().pending());
    OWNER.with(|o| o.set(cpu.owner));
    assert!(rt.dispatch(&mut Dispatcher::new()).unwrap());
    assert_eq!(cpu.data.lock().context.as_ref().unwrap().ipis, 1);
}

#[cfg(feature = "selftest")]
#[test]
fn native_idle_observation_is_single_owner_and_never_changes_the_armed_tick() {
    use super::model::{IDLE_DEADLINE, NOW};
    for harts in [1, 2] {
        let f = Fixture::new(harts, 1);
        let rt = &f.runtime;
        let mut local = Dispatcher::new();
        NOW.with(|now| now.set(500));
        let deadline = 500 + (rt.board.info.timebase_hz / 10_000).max(1);
        rt.idle(&mut local).unwrap();
        rt.idle(&mut local).unwrap();
        assert_eq!(IDLE_DEADLINE.with(|d| d.get()), Some(deadline));
        assert_eq!(
            rt.idle_wakes(),
            (harts == 1).then_some(([2, 0, 0, 0, 0], [0; 4]))
        );
    }
}

#[test]
fn high_owners_receive_distinct_kicks_and_must_both_retire_before_restart() {
    use super::model::{IDLE_DEADLINE, KICKED_OWNERS, KICKS, OWNER};
    let f = Fixture::with_config(64, [3, 1], 0, |id, vm| {
        let mut pins = [0; pauli_config::MAX_GUEST_VCPUS as usize];
        if id == 0 {
            pins[..3].copy_from_slice(&[63, 32, 63]);
        } else {
            pins[0] = 7;
        }
        vm.cpu_affinity = pauli_config::CpuAffinity::Physical(pins);
    });
    let rt = &f.runtime;
    let vm = rt.manager.vm(0).unwrap();
    let peer = rt.manager.vm(1).unwrap();
    let mut local = Dispatcher::new();
    for (owner, periodic) in [(0, true), (31, false), (32, true), (62, false), (63, true)] {
        OWNER.with(|o| o.set(owner));
        rt.idle(&mut local).unwrap();
        assert_eq!(IDLE_DEADLINE.with(|t| t.get().is_some()), periodic);
    }
    rt.manager.start(rt.board, 0).unwrap();
    rt.manager.start(rt.board, 1).unwrap();
    let entry = vm.config.guest_base as usize + pauli_config::KERNEL_OFFSET as usize;
    for i in 1..3 {
        vm.cpu(i).unwrap().request_start(entry, 0).unwrap();
    }
    for owner in [63, 32, 63, 7] {
        OWNER.with(|o| o.set(owner));
        assert!(rt.dispatch(&mut local).unwrap());
    }
    let peer_epoch = peer.epoch();
    KICKED_OWNERS.with(|m| m.set(0));
    KICKS.with(|k| k.set(0));
    vm.host_stop();
    assert_eq!(KICKS.with(|k| k.get()), 2);
    assert_eq!(
        KICKED_OWNERS.with(|m| m.get()),
        (1usize << 63) | (1usize << 32)
    );
    OWNER.with(|o| o.set(63));
    rt.dispatch(&mut local).unwrap();
    assert!(!vm.try_join(0, rt.manager.link()).unwrap());
    assert_eq!(
        pauli_control::Backend::start(
            &mut crate::monitor::backend::Target(rt),
            pauli_control::VmId(1)
        ),
        Err(pauli_control::Error::Busy)
    );
    OWNER.with(|o| o.set(32));
    rt.dispatch(&mut local).unwrap();
    assert!(vm.try_join(0, rt.manager.link()).unwrap());
    let old = vm.epoch();
    rt.manager.start(rt.board, 0).unwrap();
    assert_ne!(vm.epoch(), old);
    OWNER.with(|o| o.set(7));
    assert!(rt.dispatch(&mut local).unwrap());
    assert_eq!(peer.epoch(), peer_epoch);
}

#[test]
fn a_rejected_first_worker_ipi_faults_only_the_new_activation() {
    use super::model::{OWNER, REJECT_KICK_ONCE};
    let f = Fixture::with_config(64, [1, 1], 0, |slot, vm| {
        let mut pins = [0; pauli_config::MAX_GUEST_VCPUS as usize];
        pins[0] = if slot == 0 { 63 } else { 32 };
        vm.cpu_affinity = pauli_config::CpuAffinity::Physical(pins);
    });
    let rt = &f.runtime;
    rt.manager.start(rt.board, 1).unwrap();
    let peer = rt.manager.vm(1).unwrap();
    let old = peer.epoch();
    REJECT_KICK_ONCE.with(|cpu| cpu.set(Some(63)));
    assert_eq!(rt.manager.start(rt.board, 0), Err(crate::HvError::Fault));
    let vm = rt.manager.vm(0).unwrap();
    assert!(!vm.lifecycle.can_enter());
    assert!(vm.try_join(0, rt.manager.link()).unwrap());
    assert_eq!(vm.lifecycle.state(), crate::vm::State::Faulted);
    rt.manager.start(rt.board, 0).unwrap();
    OWNER.with(|o| o.set(32));
    assert!(rt.dispatch(&mut Dispatcher::new()).unwrap());
    assert_eq!(peer.epoch(), old);
}

#[test]
fn one_hart_time_slices_one_and_three_vcpu_guests_independently() {
    let fixture = Fixture::with_vcpus(1, [1, 3]);
    let rt = &fixture.runtime;
    let small = &rt.manager.vm(0).unwrap();
    let large = &rt.manager.vm(1).unwrap();
    assert!(small.cpu(1).is_none());
    assert!(large.cpu(3).is_none());
    let local = rt.manager.placed_on(0, 0);
    assert_eq!([local.local_cpus(0), local.local_cpus(1)], [0b1, 0b111]);
    for vm in [small, large] {
        rt.manager.start(rt.board, vm.slot).unwrap();
        let entry = vm.config.guest_base as usize + pauli_config::KERNEL_OFFSET as usize;
        for index in 1..usize::from(vm.config.vcpus) {
            vm.cpu(index).unwrap().request_start(entry, 0).unwrap();
        }
    }
    let mut hart = Dispatcher::new();
    let steps = |vm: &crate::vm::Vm<super::model::Arch>, cpu: usize| {
        vm.cpu(cpu)
            .unwrap()
            .data
            .lock()
            .context
            .as_ref()
            .unwrap()
            .steps
    };
    for _ in 0..3 {
        for (vm, cpu) in [(small, 0), (large, 0), (large, 1), (large, 2)] {
            let before = steps(vm, cpu);
            assert!(rt.dispatch(&mut hart).unwrap());
            assert!(steps(vm, cpu) > before);
        }
    }
    let epoch = small.epoch();
    let before = steps(small, 0);
    large.host_stop();
    assert!(rt.dispatch(&mut hart).unwrap());
    assert!(large.try_join(0, rt.manager.link()).unwrap());
    assert!(large.retired());
    rt.manager.start(rt.board, large.slot).unwrap();
    assert_eq!(small.epoch(), epoch);
    assert!(steps(small, 0) > before);
}
