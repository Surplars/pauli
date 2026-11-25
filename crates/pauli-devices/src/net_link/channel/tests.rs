use super::*;
use crate::{
    net_link::{DEPTH, Link},
    virtio_net::{FRAME_MAX, NetBackend},
};

fn send(ch: &mut Channel, ep: ChannelEndpoint, tag: u8) -> Option<RxReady> {
    let mut tx = ch.backend(ep).unwrap();
    tx.transmit(&[tag; FRAME_MAX]).unwrap();
    tx.ready()
}

#[test]
fn arbitrary_slots_duplex_wrap_full_and_down_counters() {
    let mut ch = Channel::new(14);
    let a = ch.bind(Side::A, 40_000, 1).unwrap();
    assert_eq!(ch.bind(Side::A, 8, 3), Err(Error::Bound));
    assert_eq!(ch.bind(Side::B, 40_000, 2), Err(Error::Invalid));
    assert_eq!(ch.bind(Side::B, 80_000, 0), Err(Error::Invalid));
    ch.activate(a).unwrap();
    assert_eq!(send(&mut ch, a, 0xff), None);
    let b = ch.bind(Side::B, 80_000, 2).unwrap();
    ch.activate(b).unwrap();
    assert_eq!(ch.peer_slot(Side::B), Some(40_000));
    assert_eq!(ch.peer_slot(Side::A), Some(80_000));
    for batch in 0..48u8 {
        for i in 0..DEPTH {
            let tag = batch.wrapping_add(i as u8);
            assert_eq!(send(&mut ch, a, tag).unwrap().slot(), 80_000);
            assert_eq!(send(&mut ch, b, !tag).unwrap().slot(), 40_000);
        }
        for ep in [a, b] {
            assert!(send(&mut ch, ep, 0).is_none());
            assert_eq!(ch.snapshot(ep).unwrap().queued, DEPTH);
        }
        for i in 0..DEPTH {
            let tag = batch.wrapping_add(i as u8);
            for (ep, expected) in [(a, !tag), (b, tag)] {
                let mut buf = [0; FRAME_MAX];
                assert_eq!(
                    ch.backend(ep).unwrap().receive(&mut buf),
                    Ok(Some(FRAME_MAX))
                );
                assert_eq!(buf, [expected; FRAME_MAX]);
            }
        }
    }
    for (ep, down) in [(a, 1), (b, 0)] {
        let s = ch.snapshot(ep).unwrap();
        assert_eq!(
            (s.tx, s.rx, s.full, s.down, s.dropped, s.up, s.queued),
            (768, 768, 48, down, 48 + down, true, 0)
        );
    }
}

#[test]
fn hints_cannot_cross_reset_either_binding_or_recycled_allocation() {
    let mut ch = Channel::new(55);
    let a = ch.bind(Side::A, 4000, 31).unwrap();
    let b = ch.bind(Side::B, 9000, 32).unwrap();
    ch.activate(a).unwrap();
    ch.activate(b).unwrap();
    let old = send(&mut ch, a, 0xf1).unwrap();
    assert!(ch.receiver(b, old).is_some());
    ch.reset(a).unwrap();
    assert_eq!(ch.snapshot(b).unwrap().queued, 0);
    let after_reset = send(&mut ch, a, 0xf2).unwrap();
    assert!(ch.receiver(b, old).is_none());
    let snapshot = ch.retire(a).unwrap();
    assert_eq!((snapshot.up, snapshot.tx, snapshot.queued), (false, 2, 0));
    assert_eq!(ch.side_snapshot(Side::A), snapshot);
    let a2 = ch.bind(Side::A, 4000, 31).unwrap();
    ch.activate(a2).unwrap();
    assert!(ch.backend(a).is_err());
    let after_a = send(&mut ch, a2, 0xf3).unwrap();
    assert!(ch.receiver(b, after_reset).is_none());
    ch.retire(b).unwrap();
    let b2 = ch.bind(Side::B, 9000, 32).unwrap();
    ch.activate(b2).unwrap();
    let after_b = send(&mut ch, a2, 0xf4).unwrap();
    assert!(ch.receiver(b, after_b).is_none());
    assert!(ch.receiver(b2, after_a).is_none());
    assert!(ch.receiver(b2, after_b).is_some());
    ch.scrub();
    for p in &ch.ports {
        assert!(
            p.frames
                .iter()
                .all(|frame| frame.len == 0 && frame.bytes.iter().all(|&v| v == 0))
        );
        assert_eq!(
            (p.head, p.len, p.generation, p.tx, p.rx, p.epoch, p.active),
            (0, 0, 0, 0, 0, 0, false)
        );
    }
    assert!(ch.bind(Side::B, 7, 1).is_err());
    ch.initialize(56);
    let a3 = ch.bind(Side::A, 4000, 31).unwrap();
    let b3 = ch.bind(Side::B, 9000, 32).unwrap();
    ch.activate(a3).unwrap();
    ch.activate(b3).unwrap();
    let fresh = send(&mut ch, a3, 1).unwrap();
    assert!(ch.receiver(b3, old).is_none()); // matching queue-generation and VM tuples
    assert!(ch.backend(a).is_err());
    assert!(ch.receiver(b3, fresh).is_some());
    let mut fixed = Link::new();
    let fixed_ep = fixed.bind(1, 32, 0).unwrap();
    assert!(fixed.receiver(fixed_ep, fresh).is_none());
    assert_ne!(Side::A.mac(0), Side::B.mac(0));
    assert_ne!(Side::A.mac(0), Side::A.mac(1));
    assert_eq!(Side::A.mac(0)[0], 0x06);
}
