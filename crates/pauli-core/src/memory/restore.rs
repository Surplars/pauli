//! Single-pass cold restoration of private RAM from immutable board templates.
//! Ownership/Lock Ordering: the initial assignment or joined VM owns the whole range.
//! Invariants: ordered zero/copy spans cover that exact assignment once, without DMA.
use super::GuestRam;
use crate::{HvError, Result};
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
                    zero_volatile(cursor, gap);
                    cursor += gap;
                }
                if !bytes.is_empty() {
                    copy_volatile(cursor, bytes.as_ptr(), bytes.len());
                    cursor += bytes.len();
                }
            }
        }
        Ok(())
    }
}

/// # Safety
/// `[dst, dst + len)` must be retained, writable foreign RAM with no running
/// guest, device DMA, or aliased Rust reference observing those bytes.
unsafe fn zero_volatile(mut dst: usize, mut len: usize) {
    while len != 0 && (dst & 7) != 0 {
        // SAFETY: caller proved `[dst, dst + len)` is writable foreign RAM.
        unsafe { (dst as *mut u8).write_volatile(0) };
        dst += 1;
        len -= 1;
    }
    while len >= 64 {
        // Address arithmetic must also work for physical RAM outside a Rust allocation.
        // SAFETY: `dst` is 8-byte aligned and at least 64 bytes remain.
        unsafe {
            let ptr = dst as *mut u64;
            ptr.write_volatile(0);
            ptr.wrapping_add(1).write_volatile(0);
            ptr.wrapping_add(2).write_volatile(0);
            ptr.wrapping_add(3).write_volatile(0);
            ptr.wrapping_add(4).write_volatile(0);
            ptr.wrapping_add(5).write_volatile(0);
            ptr.wrapping_add(6).write_volatile(0);
            ptr.wrapping_add(7).write_volatile(0);
        }
        dst += 64;
        len -= 64;
    }
    while len >= 8 {
        // SAFETY: `dst` is 8-byte aligned and at least 8 bytes remain.
        unsafe { (dst as *mut u64).write_volatile(0) };
        dst += 8;
        len -= 8;
    }
    while len != 0 {
        // SAFETY: at least 1 byte remains in the checked destination span.
        unsafe { (dst as *mut u8).write_volatile(0) };
        dst += 1;
        len -= 1;
    }
}

/// # Safety
/// `[dst, dst + len)` must be retained, writable foreign RAM disjoint from
/// readable `[src, src + len)`.
unsafe fn copy_volatile(mut dst: usize, mut src: *const u8, mut len: usize) {
    while len != 0 && (dst & 7) != 0 {
        // SAFETY: destination and source are in-bounds and disjoint.
        unsafe {
            (dst as *mut u8).write_volatile(src.read());
            src = src.add(1);
        }
        dst += 1;
        len -= 1;
    }
    if (src as usize) & 7 == 0 {
        while len >= 64 {
            // SAFETY: both `dst` and `src` are 8-byte aligned and 64 bytes remain.
            unsafe {
                let d = dst as *mut u64;
                let s = src.cast::<u64>();
                let v0 = s.read();
                let v1 = s.add(1).read();
                let v2 = s.add(2).read();
                let v3 = s.add(3).read();
                let v4 = s.add(4).read();
                let v5 = s.add(5).read();
                let v6 = s.add(6).read();
                let v7 = s.add(7).read();
                d.write_volatile(v0);
                d.wrapping_add(1).write_volatile(v1);
                d.wrapping_add(2).write_volatile(v2);
                d.wrapping_add(3).write_volatile(v3);
                d.wrapping_add(4).write_volatile(v4);
                d.wrapping_add(5).write_volatile(v5);
                d.wrapping_add(6).write_volatile(v6);
                d.wrapping_add(7).write_volatile(v7);
                src = src.add(64);
            }
            dst += 64;
            len -= 64;
        }
        while len >= 8 {
            // SAFETY: both `dst` and `src` are 8-byte aligned and 8 bytes remain.
            unsafe {
                (dst as *mut u64).write_volatile(src.cast::<u64>().read());
                src = src.add(8);
            }
            dst += 8;
            len -= 8;
        }
    } else {
        while len >= 8 {
            // SAFETY: `dst` is 8-byte aligned and `src` has at least 8 readable bytes.
            unsafe {
                (dst as *mut u64).write_volatile(src.cast::<u64>().read_unaligned());
                src = src.add(8);
            }
            dst += 8;
            len -= 8;
        }
    }
    while len != 0 {
        // SAFETY: at least 1 byte remains in both destination and source.
        unsafe {
            (dst as *mut u8).write_volatile(src.read());
            src = src.add(1);
        }
        dst += 1;
        len -= 1;
    }
}
