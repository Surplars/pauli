//! Exclusive-owner SBI deadline reuse policy. No time arithmetic: retaining
//! an expired absolute compare must leave its pending interrupt asserted.
//! Ownership/Lock Ordering: sole writer of one physical compare endpoint.
//! Invariants: only a successful endpoint call commits cached compare knowledge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Statistics {
    /// Firmware programming attempts, including failed calls.
    pub writes: u64,
    /// Identical-deadline requests satisfied without entering firmware.
    pub reuses: u64,
}

pub struct Cache {
    programmed: Option<u64>,
    statistics: Statistics,
}
impl Cache {
    pub const fn new() -> Self {
        Self {
            programmed: None,
            statistics: Statistics {
                writes: 0,
                reuses: 0,
            },
        }
    }
    pub const fn statistics(&self) -> Statistics {
        self.statistics
    }
    /// Only a successful write establishes knowledge of the firmware deadline.
    /// On error the hardware state is unknown, not eligible for later reuse.
    pub fn program<E>(
        &mut self,
        deadline: u64,
        write: impl FnOnce(u64) -> Result<(), E>,
    ) -> Result<(), E> {
        if self.programmed == Some(deadline) {
            self.statistics.reuses += 1;
            return Ok(());
        }
        self.programmed = None;
        self.statistics.writes += 1;
        write(deadline)?;
        self.programmed = Some(deadline);
        Ok(())
    }
}
