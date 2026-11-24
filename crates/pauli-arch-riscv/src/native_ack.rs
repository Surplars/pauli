//! Record positive native SSIP clears in selftest, never WFI returns or untagged traps.
//! Only the IRQ-masked owner writes its slot and reads its payload. Other harts
//! read just the published sequence, not a racing cross-hart timestamp snapshot.
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use pauli_core::arch::{NativeAck, NativeAckPath};

struct Slot {
    sequence: AtomicU64,
    ticks: AtomicU64,
    guest_exit: AtomicBool,
}
pub(crate) struct Receipts([Slot; crate::MAX_CPUS]);
#[cfg(target_arch = "riscv64")]
pub(crate) static RECEIPTS: Receipts = Receipts::new();

impl Receipts {
    const fn new() -> Self {
        Self(
            [const {
                Slot {
                    sequence: AtomicU64::new(0),
                    ticks: AtomicU64::new(0),
                    guest_exit: AtomicBool::new(false),
                }
            }; crate::MAX_CPUS],
        )
    }
    #[inline]
    pub(crate) fn observe(
        &self,
        old_sip: usize,
        path: NativeAckPath,
        owner: impl FnOnce() -> usize,
        clock: impl FnOnce() -> u64,
    ) {
        if old_sip & crate::bits::SSIP == 0 {
            return; // Ordinary empty scan: no TIME read or shared counter access.
        }
        let ticks = clock();
        let slot = &self.0[owner()];
        let next = slot.sequence.load(Ordering::Relaxed).wrapping_add(1);
        slot.ticks.store(ticks, Ordering::Relaxed);
        slot.guest_exit
            .store(path == NativeAckPath::GuestExit, Ordering::Relaxed);
        slot.sequence.store(next, Ordering::Release);
    }
    pub(crate) fn sequence(&self, owner: usize) -> u64 {
        self.0[owner].sequence.load(Ordering::Acquire)
    }
    pub(crate) fn local(&self, owner: usize) -> Option<NativeAck> {
        let slot = &self.0[owner];
        let sequence = slot.sequence.load(Ordering::Relaxed);
        (sequence != 0).then(|| NativeAck {
            sequence,
            ticks: slot.ticks.load(Ordering::Relaxed),
            path: if slot.guest_exit.load(Ordering::Relaxed) {
                NativeAckPath::GuestExit
            } else {
                NativeAckPath::DispatchAck
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::{SEIP, SSIP, STIP};
    use NativeAckPath::*;

    #[test]
    fn a_clear_without_ssip_never_touches_the_owner_or_clock() {
        let log = Receipts::new();
        for old in [0, SEIP, STIP, SEIP | STIP] {
            log.observe(
                old,
                DispatchAck,
                || panic!("no owner lookup"),
                || panic!("no clock"),
            );
        }
        assert_eq!((log.sequence(0), log.local(0)), (0, None));
    }

    #[test]
    fn ack_paths_publish_once_per_positive_clear_independently_per_owner() {
        let log = Receipts::new();
        log.observe(SSIP, GuestExit, || 1, || 0);
        log.observe(0, DispatchAck, || 1, || panic!("already cleared"));
        assert_eq!(
            log.local(1),
            Some(NativeAck {
                sequence: 1,
                ticks: 0,
                path: GuestExit
            })
        );
        log.observe(SSIP | STIP, DispatchAck, || 0, || 22);
        log.observe(SSIP, DispatchAck, || 1, || 34);
        assert_eq!((log.sequence(0), log.sequence(1)), (1, 2));
        assert_eq!(
            log.local(0),
            Some(NativeAck {
                sequence: 1,
                ticks: 22,
                path: DispatchAck
            })
        );
        assert_eq!(
            log.local(1),
            Some(NativeAck {
                sequence: 2,
                ticks: 34,
                path: DispatchAck
            })
        );
    }
}
