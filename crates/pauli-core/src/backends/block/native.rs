//! Adapter-local native handles; asynchronous calls retain no caller slices.
//! Ownership/Lock Ordering: VM devices -> native driver, never reverse callbacks.
//! Invariants: one operation per trusted slot; completion frees its exact native token.
use super::*;
#[derive(Clone, Copy)]
pub(super) struct Pending {
    pub token: BlockToken,
    pub bytes: usize,
    pub deadline: u64,
    pub operation: BlockOperation,
}
pub(super) struct Backend {
    pub native: &'static dyn HostBlockDevice,
    pub event: &'static CompletionEvent,
    pub pending: [Option<Pending>; MAX_INFLIGHT],
    pub admission: [Option<u64>; MAX_INFLIGHT],
    pub now: u64,
    pub submitted: u64,
    batch: usize,
    pub timeout: u64,
    pub quarantined: bool,
}
impl Backend {
    pub fn new(
        native: &'static dyn HostBlockDevice,
        event: &'static CompletionEvent,
        batch: usize,
        timeout: u64,
    ) -> Self {
        Self {
            native,
            event,
            batch,
            timeout,
            now: 0,
            submitted: 0,
            pending: [None; MAX_INFLIGHT],
            admission: [None; MAX_INFLIGHT],
            quarantined: false,
        }
    }
    pub fn deadline(&self) -> Option<u64> {
        self.pending
            .iter()
            .flatten()
            .map(|p| p.deadline)
            .chain(self.admission.iter().flatten().copied())
            .min()
    }
    fn start(
        &mut self,
        slot: usize,
        operation: BlockOperation,
        sector: u64,
        bytes: usize,
        data: &[u8],
    ) -> Poll<bool> {
        if self.quarantined {
            return Poll::Ready(false);
        }
        if self.pending[slot].is_some() {
            return Poll::Ready(true);
        }
        let deadline = *self.admission[slot].get_or_insert(self.now.saturating_add(self.timeout));
        let request = BlockSubmission {
            operation,
            sector,
            bytes,
            deadline,
            event: Some(self.event),
        };
        match self.native.submit(request, data) {
            Ok(token) => {
                self.pending[slot] = Some(Pending {
                    token,
                    bytes,
                    deadline,
                    operation,
                });
                self.admission[slot] = None;
                self.submitted += 1;
                Poll::Ready(true)
            }
            Err(DeviceError::Busy) if self.now < deadline => Poll::Pending,
            Err(_) => {
                self.admission[slot] = None;
                Poll::Ready(false)
            }
        }
    }
    fn poll(&mut self, slot: usize, output: &mut [u8]) -> Poll<bool> {
        let Some(pending) = self.pending[slot] else {
            return Poll::Ready(false);
        };
        let status = self.native.try_poll(pending.token);
        if status == Ok(None) {
            return Poll::Pending;
        }
        let collected = self.native.collect(pending.token, output);
        let okay = status == Ok(Some(pending.bytes)) && collected == Ok(pending.bytes);
        if !okay {
            // A failed read can only affect driver-owned bounce memory. Never
            // hand a volume to another owner after an unconfirmed mutation.
            if pending.operation == BlockOperation::Read {
                let _ = self.native.cancel(pending.token);
            } else {
                self.quarantined = true;
            }
        }
        self.pending[slot] = None;
        Poll::Ready(okay)
    }
    pub fn drain_all(&mut self) -> Result<bool> {
        if self.quarantined {
            return Err(HvError::Fault);
        }
        self.admission.fill(None);
        let mut done = true;
        for slot in 0..MAX_INFLIGHT {
            let Some(pending) = self.pending[slot] else {
                continue;
            };
            if pending.operation == BlockOperation::Read {
                self.native.cancel(pending.token)?;
                self.pending[slot] = None;
            } else {
                match self.poll(slot, &mut []) {
                    Poll::Pending => done = false,
                    Poll::Ready(true) => {}
                    Poll::Ready(false) => return Err(HvError::Fault),
                }
            }
        }
        Ok(done)
    }
}
impl BlockBackend for Backend {
    fn sectors(&self) -> u64 {
        self.native.sectors()
    }
    fn max_transfer_bytes(&self) -> usize {
        self.batch
    }
    fn read(&mut self, slot: usize, sector: u64, output: &mut [u8]) -> Poll<bool> {
        match self.start(slot, BlockOperation::Read, sector, output.len(), &[]) {
            Poll::Ready(true) => self.poll(slot, output),
            other => other,
        }
    }
    fn write(&mut self, slot: usize, sector: u64, input: &[u8]) -> Poll<bool> {
        match self.start(slot, BlockOperation::Write, sector, input.len(), input) {
            Poll::Ready(true) => self.poll(slot, &mut []),
            other => other,
        }
    }
    fn flush(&mut self, slot: usize) -> Poll<bool> {
        match self.start(slot, BlockOperation::Flush, 0, 0, &[]) {
            Poll::Ready(true) => self.poll(slot, &mut []),
            other => other,
        }
    }
    fn drain(&mut self, slot: usize, scratch: &mut [u8]) -> Poll<()> {
        self.admission[slot] = None;
        if self.pending[slot].is_none() {
            return Poll::Ready(());
        }
        self.poll(slot, scratch).map(|_| ())
    }
}
