//! One VM's interrupt chip and (selftest only) its latest UART timing pair.
//! Ownership: the existing IRQ lock orders the producer, the pinned owner and status.
//! Invariant: only the first owner sample of an unambiguous RX edge can timestamp it.
#[cfg(feature = "selftest")]
use crate::arch::NativeAck;
use crate::arch::{ArchSmp, VirtualIrqChip};
use core::ops::{Deref, DerefMut};
use pauli_config::VmConfig;

pub(crate) struct IrqBank<I> {
    chip: I,
    #[cfg(feature = "selftest")]
    pub(crate) rx: RxObservation,
}

impl<I> Deref for IrqBank<I> {
    type Target = I;
    fn deref(&self) -> &I {
        &self.chip
    }
}
impl<I> DerefMut for IrqBank<I> {
    fn deref_mut(&mut self) -> &mut I {
        &mut self.chip
    }
}

impl<I: VirtualIrqChip> IrqBank<I> {
    pub(super) fn new(chip: I, _config: &VmConfig) -> Self {
        Self {
            chip,
            #[cfg(feature = "selftest")]
            rx: RxObservation {
                // The timing fixture has one PLIC context and a single UART producer.
                // An aggregate line is not a per-source receipt for a generic guest.
                probe: RxObservation::enabled(_config),
                ..Default::default()
            },
        }
    }

    /// Same chip lock as the pending read and any resident-file transfer.
    /// A waiting owner may observe the edge on its wake check before entry.
    #[inline]
    pub(crate) fn owner_pending<S: ArchSmp>(
        &mut self,
        vcpu: u8,
        _owner: usize,
        _clock: impl FnOnce() -> u64,
    ) -> bool {
        let pending = self.chip.pending(vcpu);
        #[cfg(feature = "selftest")]
        if self.rx.awaiting && vcpu == 0 && pending {
            if let Some(rise) = &mut self.rx.last {
                rise.owner = Some(OwnerSample {
                    vcpu,
                    owner: _owner,
                    ticks: _clock(),
                    ack: S::local_ack(),
                });
            }
            self.rx.awaiting = false;
        }
        pending
    }
}

#[cfg(feature = "selftest")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OwnerSample {
    pub vcpu: u8,
    pub owner: usize,
    pub ticks: u64,
    ack: Option<NativeAck>,
}
#[cfg(feature = "selftest")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RxRise {
    pub seq: u64,
    pub byte: u8,
    pub ticks: u64,
    pub owner: Option<OwnerSample>,
    send: Option<Send>,
}
#[cfg(feature = "selftest")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Send {
    owner: usize,
    baseline: u64,
    ticks: u64,
    returned: u64,
}
#[cfg(feature = "selftest")]
impl RxRise {
    /// SSIP is shared: this brackets a real native ACK, not a unique interrupt source.
    pub(crate) fn paired_ack(self) -> Option<(u64, NativeAck, u64)> {
        let (owner, send) = (self.owner?, self.send?);
        let ack = owner.ack?;
        (owner.owner == send.owner
            && ack.sequence == send.baseline.checked_add(1)?
            && self.ticks <= send.ticks
            && send.ticks <= send.returned
            && send.ticks < ack.ticks
            && ack.ticks <= owner.ticks)
            .then_some((send.ticks, ack, send.returned))
    }
}

/// Unpaired -> candidate on RX rise -> observed on first owner pending read.
/// An intervening chip transaction cancels a candidate rather than guessing
/// which source owns a shared interrupt line.
#[cfg(feature = "selftest")]
#[derive(Default)]
pub(crate) struct RxObservation {
    probe: bool,
    pub(super) awaiting: bool,
    pub(super) solo_wake: bool,
    pub(crate) last: Option<RxRise>,
}
#[cfg(feature = "selftest")]
impl RxObservation {
    pub(super) fn enabled(config: &VmConfig) -> bool {
        config.firmware.is_some() && config.vcpus == 1 && !config.aia && !config.control_events
    }
    pub(super) fn record_rise(&mut self, byte: u8, ticks: u64) {
        let seq = self.last.map_or(1, |r| r.seq.wrapping_add(1));
        self.last = Some(RxRise {
            seq,
            byte,
            ticks,
            owner: None,
            send: None,
        });
        self.awaiting = self.probe;
    }
    /// Called under the same lock as the wake-bit swap. A kick already pending
    /// at the edge, a retry, or a line the owner already saw gets no ticket.
    pub(super) fn take_send(
        &mut self,
        epoch: u32,
        owner: usize,
        sequence: impl FnOnce(usize) -> u64,
    ) -> Option<SendTicket> {
        if !core::mem::take(&mut self.solo_wake) || !self.awaiting {
            return None;
        }
        let seq = self.last?.seq;
        let baseline = sequence(owner);
        Some(SendTicket {
            epoch,
            seq,
            owner,
            baseline,
        })
    }
    pub(super) fn confirm(&mut self, epoch: u32, ticket: SendTicket, sent: u64, returned: u64) {
        if let Some(r) = &mut self.last
            && epoch == ticket.epoch
            && r.seq == ticket.seq
        {
            r.send = Some(Send {
                owner: ticket.owner,
                baseline: ticket.baseline,
                ticks: sent,
                returned,
            });
        }
    }
}
#[cfg(feature = "selftest")]
pub(super) struct SendTicket {
    epoch: u32,
    seq: u64,
    owner: usize,
    baseline: u64,
}

#[cfg(all(test, feature = "selftest"))]
mod tests {
    use super::*;
    use crate::arch::NativeAckPath;

    fn edge() -> RxObservation {
        let mut rx = RxObservation {
            probe: true,
            ..Default::default()
        };
        rx.record_rise(b'p', 10);
        rx.solo_wake = true;
        rx
    }

    #[test]
    fn successful_sender_and_receiver_intervals_can_overlap() {
        let owner = 1;
        let ack = NativeAck {
            sequence: 8,
            ticks: 12,
            path: NativeAckPath::GuestExit,
        };
        // Sender returns before, on, or after the ACK tick; 18 is even
        // later than the receiver's owner sample. Only its own TIME is ordered.
        for returned in [10, 11, 12, 13, 18] {
            let mut rx = edge();
            let ticket = rx.take_send(9, owner, |_| 7).unwrap();
            rx.last.as_mut().unwrap().owner = Some(OwnerSample {
                vcpu: 0,
                owner,
                ticks: 15,
                ack: Some(ack),
            });
            rx.awaiting = false;
            assert_eq!(rx.last.unwrap().paired_ack(), None);
            rx.confirm(9, ticket, 11, returned);
            assert_eq!(
                rx.last.unwrap().paired_ack(),
                (11 <= returned).then_some((11, ack, returned))
            );
        }
    }

    #[test]
    fn late_sender_return_cannot_confirm_another_epoch_or_byte() {
        let owner = 1;
        let mut old = edge();
        let stale = old.take_send(9, owner, |_| 11).unwrap();
        let mut restarted = edge(); // Same RX seq and VM slot, different epoch.
        restarted.confirm(10, stale, 15, 20);
        assert!(restarted.last.unwrap().send.is_none());

        let stale = restarted.take_send(10, owner, |_| 11).unwrap();
        restarted.record_rise(b'q', 17); // Another RX won the race before SBI returned.
        restarted.confirm(10, stale, 16, 20);
        assert!(restarted.last.unwrap().send.is_none());
    }
}
