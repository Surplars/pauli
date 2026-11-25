//! Producer-side registrations are bounded by the immutable boot slots. A
//! dynamic child cannot opt into control events, but can be observed by every
//! eligible boot VM regardless of its own slot number. The producer I/O lock
//! serializes grant changes with history writes. Delivery uses a copied batch.
use super::State;
use pauli_config::MAX_BOOT_VMS;
use pauli_control::abi::{EVENT_LOG, EVENT_STATE};

const _: () = assert!(MAX_BOOT_VMS <= u8::BITS as usize);

#[derive(Default)]
pub(crate) struct Observers {
    epochs: [u32; MAX_BOOT_VMS],
    logs: u8,
    states: u8,
    log_pending: u8,
    state_pending: u8,
    last_state: Option<State>,
}

impl Observers {
    pub(crate) fn active(&self) -> bool {
        self.logs | self.states != 0
    }
    /// Commit the old batch before adding a watcher. Earlier writes cannot
    /// become newly observable when a grant is installed mid-batch.
    pub(crate) fn record(&mut self, log_dirty: bool, state: State) {
        if log_dirty {
            self.log_pending |= self.logs;
        }
        if self.last_state != Some(state) {
            self.last_state = Some(state);
            self.state_pending |= self.states;
        }
    }

    pub(crate) fn set(&mut self, slot: usize, epoch: u32, classes: u64, replaced: bool) {
        let Some(owner) = self.epochs.get_mut(slot) else {
            return;
        };
        let bit = 1 << slot;
        if *owner != epoch {
            if classes == 0 {
                return;
            }
            self.log_pending &= !bit;
            self.state_pending &= !bit;
        }
        *owner = epoch;
        self.logs = (self.logs & !bit) | (u8::from(classes & EVENT_LOG != 0) << slot);
        self.states = (self.states & !bit) | (u8::from(classes & EVENT_STATE != 0) << slot);
        self.log_pending &= self.logs;
        self.state_pending &= self.states;
        if replaced {
            self.state_pending |= self.states & bit;
        }
    }

    pub(crate) fn take(&mut self) -> Option<Batch> {
        (self.log_pending | self.state_pending != 0).then(|| Batch {
            epochs: self.epochs,
            logs: core::mem::take(&mut self.log_pending),
            states: core::mem::take(&mut self.state_pending),
        })
    }
}

pub(crate) struct Batch {
    epochs: [u32; MAX_BOOT_VMS],
    logs: u8,
    states: u8,
}
impl Batch {
    pub(crate) fn recipients(self) -> impl Iterator<Item = (usize, u32, u64)> {
        self.epochs
            .into_iter()
            .enumerate()
            .filter_map(move |(slot, epoch)| {
                let bit = 1 << slot;
                let classes = u64::from(self.logs & bit != 0) * EVENT_LOG
                    | u64::from(self.states & bit != 0) * EVENT_STATE;
                (classes != 0).then_some((slot, epoch, classes))
            })
    }
}
