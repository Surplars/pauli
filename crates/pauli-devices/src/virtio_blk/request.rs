//! One captured guest request, resumed only by the VM's device owner. At most
//! one backend operation is active PER SLOT. The queue owner serializes flush
//! barriers; independent data requests may finish in any order.
//! Ownership/Lock Ordering: one device owner, then backend; no internal locks.
//! Invariants: a pending slot retains its captured chain and staged write bytes.
//! Lifecycle: Data -> [Flush] -> Complete; reset/fault -> backend drain (no guest write).
use super::*;

enum Phase {
    Data,
    Flush,
    Complete,
}
pub(super) struct Request {
    pub head: u16,
    chain: Chain,
    pub kind: u32,
    sector: u64,
    len: usize,
    pub done: usize,
    pub flushed: bool,
    writethrough: bool,
    staged: bool,
    status: u8,
    phase: Phase,
}

impl<B: AsMut<[u8]>> VirtioBlock<B> {
    pub(super) fn request(
        &self,
        head: u16,
        chain: Chain,
        memory: &impl GuestMemory,
    ) -> Option<Request> {
        let mut header = [0u8; 16];
        if !chain.read(memory, 0, &mut header) {
            return None;
        }
        let kind = le32(&header, 0);
        let sector = le64(&header, 8);
        let mut status = match kind {
            0 => 0,
            1 => u8::from(!self.writable),
            4 if self.driver_features & FLUSH != 0 => 0,
            _ => 2,
        };
        let mut len = 0;
        let mut phase = Phase::Complete;
        if kind <= 1 {
            let read = kind == 0;
            len = if read {
                chain.writable - 1
            } else {
                chain.readable - 16
            };
            if (read && chain.readable != 16)
                || (!read && chain.writable != 1)
                || !chain.data_limits(read, if read { 0 } else { 16 }, len)
            {
                return None;
            }
            if len % 512 != 0
                || sector
                    .checked_add((len / 512) as u64)
                    .is_none_or(|end| end > self.sectors)
            {
                status = 1;
            }
            if status == 0 {
                phase = Phase::Data;
            }
        } else if kind == 4 && status == 0 {
            if chain.readable != 16 || chain.writable != 1 {
                return None;
            }
            phase = Phase::Flush;
        }
        Some(Request {
            head,
            chain,
            kind,
            sector,
            len,
            done: 0,
            flushed: false,
            writethrough: kind == 1 && self.driver_features & FLUSH == 0,
            staged: false,
            status,
            phase,
        })
    }
}

impl Request {
    pub(super) fn barrier(&self) -> bool {
        self.kind == 4 || self.writethrough
    }

    pub(super) fn progress(
        &mut self,
        slot: usize,
        memory: &mut impl GuestMemory,
        backend: &mut impl BlockBackend,
        buffer: &mut [u8],
    ) -> Option<Poll<u32>> {
        let request = self;
        loop {
            match request.phase {
                Phase::Data if request.done == request.len => {
                    request.phase = if request.writethrough {
                        Phase::Flush // Writethrough: no successful used entry before durability.
                    } else {
                        Phase::Complete
                    };
                }
                Phase::Data => {
                    if request
                        .progress_data(slot, memory, backend, buffer)?
                        .is_pending()
                    {
                        return Some(Poll::Pending);
                    }
                }
                Phase::Flush => match backend.flush(slot) {
                    Poll::Pending => return Some(Poll::Pending),
                    Poll::Ready(ok) => {
                        if ok {
                            request.flushed = true;
                        } else {
                            request.status = 1;
                        }
                        request.phase = Phase::Complete;
                    }
                },
                Phase::Complete => {
                    if !request
                        .chain
                        .write(memory, request.chain.writable - 1, &[request.status])
                    {
                        return None;
                    }
                    return Some(Poll::Ready(
                        1 + if request.kind == 0 {
                            request.done as u32
                        } else {
                            0
                        },
                    ));
                }
            }
        }
    }

    fn progress_data(
        &mut self,
        slot: usize,
        memory: &mut impl GuestMemory,
        backend: &mut impl BlockBackend,
        buffer: &mut [u8],
    ) -> Option<Poll<()>> {
        let request = self;
        let read = request.kind == 0;
        let count = (request.len - request.done)
            .min(backend.max_transfer_bytes())
            .min(buffer.len());
        let lba = request.sector + (request.done / 512) as u64;
        if read {
            let mut copy_failed = false;
            let direct = backend.read_with(slot, lba, count, &mut |data| {
                copy_failed = !request.chain.write_payload(memory, request.done, data);
                !copy_failed
            });
            // Revoked RAM breaks the queue, rather than completing with I/O error.
            if copy_failed {
                return None;
            }
            if let Some(result) = direct {
                match result {
                    Poll::Pending => return Some(Poll::Pending),
                    Poll::Ready(false) => {
                        request.status = 1;
                        request.phase = Phase::Complete;
                    }
                    Poll::Ready(true) => request.done += count,
                }
                return Some(Poll::Ready(()));
            }
        }
        // Snapshot a write chunk once; guest edits cannot change pending I/O.
        if !read && !request.staged {
            request.stage_write(memory, &mut buffer[..count])?;
        }
        let result = if read {
            backend.read(slot, lba, &mut buffer[..count])
        } else {
            backend.write(slot, lba, &buffer[..count])
        };
        match result {
            Poll::Pending => return Some(Poll::Pending),
            Poll::Ready(false) => {
                request.status = 1;
                request.phase = Phase::Complete;
            }
            Poll::Ready(true) => {
                if read
                    && !request
                        .chain
                        .write_payload(memory, request.done, &buffer[..count])
                {
                    return None;
                }
                request.done += count;
                request.staged = false;
            }
        }
        Some(Poll::Ready(()))
    }

    fn stage_write(&mut self, memory: &impl GuestMemory, buffer: &mut [u8]) -> Option<()> {
        if !self.chain.read_payload(memory, 16 + self.done, buffer) {
            return None;
        }
        self.staged = true;
        Some(())
    }
}
