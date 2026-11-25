//! Bounded, copied messages in arrival order. The receiver's I/O lock owns the inbox.
use pauli_control::{Error, MailChunk, MailMessage, VmId};

const INBOX_DEPTH: usize = 16;
const PER_SENDER_CAP: usize = 8;

#[derive(Clone, Copy)]
struct Entry {
    sender: VmId,
    epoch: u32,
    message: MailMessage,
    hint: bool,
}

pub(crate) struct Mailbox {
    entries: [Entry; INBOX_DEPTH],
    len: usize,
}

impl Mailbox {
    pub(crate) const fn new() -> Self {
        Self {
            entries: [Entry {
                sender: VmId(0),
                epoch: 0,
                hint: false,
                message: MailMessage {
                    tag: 0,
                    len: 0,
                    data: [0; 6],
                },
            }; INBOX_DEPTH],
            len: 0,
        }
    }

    pub(crate) fn send(
        &mut self,
        sender: VmId,
        epoch: u32,
        message: MailMessage,
        hint: bool,
        current: impl Fn(VmId, u32) -> bool,
    ) -> Result<(), Error> {
        if self.len == INBOX_DEPTH {
            // A destroyed or replaced sender can never again be named by a valid grant.
            // Keep stopped/faulted peers so their final messages are still readable.
            let mut kept = 0;
            for index in 0..self.len {
                let entry = self.entries[index];
                if current(entry.sender, entry.epoch) {
                    self.entries[kept] = entry;
                    kept += 1;
                }
            }
            self.len = kept;
        }
        if self.len == INBOX_DEPTH
            || self.entries[..self.len]
                .iter()
                .filter(|e| e.sender == sender && e.epoch == epoch)
                .count()
                >= PER_SENDER_CAP
        {
            return Err(Error::Busy);
        }
        self.entries[self.len] = Entry {
            sender,
            epoch,
            message,
            hint,
        };
        self.len += 1;
        Ok(())
    }

    pub(crate) fn recv(&mut self, peer: VmId, epoch: u32) -> MailChunk {
        let mut matches = self.entries[..self.len]
            .iter()
            .enumerate()
            .filter(|(_, e)| e.sender == peer && e.epoch == epoch);
        let Some((index, entry)) = matches.next() else {
            return MailChunk {
                message: None,
                more: false,
            };
        };
        let result = MailChunk {
            message: Some(entry.message),
            more: matches.next().is_some(),
        };
        self.entries.copy_within(index + 1..self.len, index);
        self.len -= 1;
        result
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::new();
    }

    pub(crate) fn has_hint(&self) -> bool {
        self.entries[..self.len].iter().any(|entry| entry.hint)
    }

    pub(crate) fn clear_hints(&mut self) {
        for entry in &mut self.entries[..self.len] {
            entry.hint = false;
        }
    }

    pub(crate) fn retract_hints(&mut self, peer: VmId, epoch: u32) {
        for entry in &mut self.entries[..self.len] {
            if entry.sender == peer && entry.epoch == epoch {
                entry.hint = false;
            }
        }
    }
}
