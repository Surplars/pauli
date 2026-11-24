//! Physical ACKs belong to a hart. Only a sent, unambiguous RX wake can use one.
//! The fixture drives the publishing and receiving owners in explicit order.
use super::*;
use crate::arch::{ArchSmp, NativeAckPath};
use crate::tests::model::Smp;

#[test]
fn only_one_new_ack_after_the_matching_send_splits_a_scored_edge() {
    for (reject, coalesced, acks, at, paired) in [
        (false, false, 1, 11, true),
        (false, false, 1, 10, false), // Equal TIME ticks cannot prove send preceded ACK.
        (false, false, 0, 11, false),
        (false, false, 2, 11, false), // Sources share SSIP; do not choose a favourite.
        (true, false, 1, 11, false),  // Failed physical request; retry must not resurrect it.
        (false, true, 1, 11, false),  // Another VM-local device wake already occupied this lane.
    ] {
        let f = pair();
        let rt = &f.runtime;
        let vm = rt.manager.vm(0).unwrap();
        OWNER.with(|o| o.set(1));
        Smp::acknowledge_kick(); // Prior start kick, before the window.
        OWNER.with(|o| o.set(0));
        reg(&f, IER, Some(1));
        if coalesced {
            vm.devices
                .wake
                .fetch_or(1, core::sync::atomic::Ordering::Release);
        }
        if reject {
            REJECT_KICK_ONCE.with(|k| k.set(Some(1)));
        }
        NOW.with(|n| n.set(10));
        Target(rt).input(VmId(1), b'p').unwrap();
        if reject {
            vm.deliver_wakes();
        }
        OWNER.with(|o| o.set(1));
        NOW.with(|n| n.set(at));
        for index in 0..acks {
            if index != 0 {
                Smp::kick(1).unwrap();
            }
            Smp::acknowledge_kick();
        }
        let mut bank = vm.devices.irqs.lock();
        assert!(bank.owner_pending::<Smp>(0, 1, || 20));
        let rise = bank.rx.last.unwrap();
        assert_eq!(rise.owner.unwrap().ticks, 20);
        let mark = rise
            .paired_ack()
            .map(|(sent, ack, returned)| (sent, ack.ticks, ack.path, returned));
        assert_eq!(
            mark,
            paired.then_some((10, at, NativeAckPath::DispatchAck, 10))
        );
    }
}

#[test]
fn an_owner_that_sees_the_line_before_send_keeps_only_the_original_interval() {
    let f = pair();
    let vm = f.runtime.manager.vm(0).unwrap();
    reg(&f, IER, Some(1));
    assert!(vm.devices.receive_console(b'p', || 10));
    OWNER.with(|o| o.set(1));
    assert!(vm.devices.irqs.lock().owner_pending::<Smp>(0, 1, || 11));
    OWNER.with(|o| o.set(0));
    NOW.with(|t| t.set(12));
    vm.deliver_wakes();
    OWNER.with(|o| o.set(1));
    NOW.with(|t| t.set(13));
    Smp::acknowledge_kick();
    let mut bank = vm.devices.irqs.lock();
    assert!(bank.owner_pending::<Smp>(0, 1, || 14));
    let rise = bank.rx.last.unwrap();
    assert_eq!(
        (rise.ticks, rise.owner.unwrap().ticks, rise.paired_ack()),
        (10, 11, None)
    );
}
