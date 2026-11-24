//! Single-pass cold restoration of private RAM from immutable board templates.
//! Ownership/Lock Ordering: the initial assignment or joined VM owns the whole range.
//! Invariants: ordered zero/copy spans cover that exact assignment once, without DMA.
use super::GuestRam;
use crate::{HvError, Result};
use core::intrinsics::{volatile_copy_nonoverlapping_memory, volatile_set_memory};
use pauli_hal::mem::PhysRange;

#[cfg(test)]
mod tests;

pub(crate) struct RamImage {
    backing: PhysRange,
    // Each template follows its zero gap. The final, empty template clears the tail.
    spans: [(usize, &'static [u8]); 4],
}

impl RamImage {
    /// Only a permanent boot template may lend sources. A removable child's DTB
    /// belongs to its recyclable arena, so a transient VM pin must not export it.
    pub(crate) fn sources(&'static self) -> crate::board::BootImages {
        crate::board::BootImages {
            kernel: self.spans[0].1,
            initrd: self.spans[1].1,
            dtb: self.spans[2].1,
        }
    }
}

impl GuestRam {
    /// Validate all template offsets before any write. Empty (optional) images
    /// consume no bytes: the next gap still covers their entire reserved space.
    pub(crate) fn plan_image(&self, parts: [(u64, &'static [u8]); 3]) -> Result<RamImage> {
        let mut spans = [(0, &[][..]); 4];
        let mut end = 0;
        for ((offset, bytes), span) in parts.into_iter().zip(&mut spans) {
            let start = usize::try_from(offset).map_err(|_| HvError::InvalidImage)?;
            let limit = start
                .checked_add(bytes.len())
                .filter(|&limit| limit <= self.len())
                .ok_or(HvError::InvalidImage)?;
            if !bytes.is_empty() {
                *span = (start.checked_sub(end).ok_or(HvError::InvalidImage)?, bytes);
                end = limit;
            }
        }
        spans[3].0 = self.len() - end;
        Ok(RamImage {
            backing: self.host,
            spans,
        })
    }

    /// # Safety
    /// Initial inactive assignment, or all vCPUs have exited, every visited native
    /// owner retired translations, and all device producers and requests are joined.
    pub(crate) unsafe fn restore_retired(&mut self, image: &RamImage) -> Result<()> {
        if self.host != image.backing {
            return Err(HvError::InvalidMemory);
        }
        let mut cursor = self.host.base();
        for &(gap, bytes) in &image.spans {
            // SAFETY: the plan partitions this checked backing; the caller proves
            // quiescence and the board retains disjoint, immutable source slices.
            // No running guest/device is observing fields with a required access width.
            unsafe {
                if gap != 0 {
                    volatile_set_memory(cursor as *mut u8, 0, gap);
                    cursor += gap;
                }
                if !bytes.is_empty() {
                    volatile_copy_nonoverlapping_memory(
                        cursor as *mut u8,
                        bytes.as_ptr(),
                        bytes.len(),
                    );
                    cursor += bytes.len();
                }
            }
        }
        Ok(())
    }
}
