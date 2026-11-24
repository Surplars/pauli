//! Host stop barrier; a completed stop includes a final native durable flush.
//! Ownership/Lock Ordering: joined VM device owner -> nonblocking native handle.
//! Invariants: neither a busy device nor a failed flush authorizes epoch reuse.
//! State: Live -> Draining -> Flushing -> Joined; errors remain non-Joined.
use super::*;
pub(super) enum Retirement {
    Live,
    Draining { deadline: u64 },
    Flushing { token: BlockToken },
    Joined,
}
impl Block {
    pub fn stop(&mut self, now: u64) -> Result<bool> {
        self.schedule.idle();
        self.backend.now = now;
        if matches!(self.retirement, Retirement::Joined) {
            return Ok(true);
        }
        if matches!(self.retirement, Retirement::Live) {
            self.retirement = Retirement::Draining {
                deadline: now.saturating_add(self.backend.timeout),
            };
        }
        if let Retirement::Draining { deadline } = self.retirement {
            if !self.backend.drain_all()? {
                return Ok(false);
            }
            if self.writable {
                let request = BlockSubmission {
                    operation: BlockOperation::Flush,
                    sector: 0,
                    bytes: 0,
                    deadline,
                    event: Some(self.backend.event),
                };
                match self.backend.native.submit(request, &[]) {
                    Ok(token) => self.retirement = Retirement::Flushing { token },
                    Err(DeviceError::Busy) if now < deadline => return Ok(false),
                    Err(error) => return Err(error.into()),
                }
            } else {
                return self.finish_stop();
            }
        }
        if let Retirement::Flushing { token } = self.retirement {
            match self.backend.native.try_poll(token) {
                Ok(None) => return Ok(false),
                Ok(Some(0)) if self.backend.native.collect(token, &mut [])? == 0 => {}
                _ => return Err(HvError::Fault),
            }
            return self.finish_stop();
        }
        Ok(false)
    }
    fn finish_stop(&mut self) -> Result<bool> {
        self.device
            .write_word(REG_STATUS, 0)
            .ok_or(HvError::InvalidDevice)?;
        self.retirement = Retirement::Joined;
        self.draining = false;
        Ok(true)
    }
}
