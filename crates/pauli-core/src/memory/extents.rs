//! Page-aligned free extents after the firmware allocator has retired.
//! Ownership/Lock Ordering: the domain pool owns this list under one cold-path lock.
//! Invariants: sorted, disjoint, coalesced runs; headers live only inside FREE RAM.
use pauli_hal::{addr::PAGE_SIZE, mem::PhysRange};
#[cfg(test)]
mod tests;

type Link = Option<&'static mut Run>;
struct Run {
    end: usize,
    next: Link,
}
const _: () = assert!(size_of::<Run>() <= PAGE_SIZE && align_of::<Run>() <= PAGE_SIZE);
impl Run {
    fn base(&self) -> usize {
        core::ptr::from_ref(self).addr()
    }
}
pub(super) struct Extents {
    first: Link,
}
impl Extents {
    /// # Safety
    /// Gaps arrive in ascending order, disjoint. Each is retained, identity-mapped
    /// writable RAM with no other owner for the lifetime of the allocations.
    pub(super) unsafe fn from_gaps(gaps: impl Iterator<Item = PhysRange>) -> Self {
        let mut out = Self { first: None };
        let mut tail = &mut out.first;
        for gap in gaps {
            let Some(base) = gap
                .base()
                .max(PAGE_SIZE)
                .checked_next_multiple_of(PAGE_SIZE)
            else {
                continue;
            };
            let end = gap.end() & !(PAGE_SIZE - 1);
            if base < end {
                // SAFETY: first full page of this exclusively transferred, ascending gap.
                tail = &mut tail.insert(unsafe { header(base, end, None) }).next;
            }
        }
        out
    }
    /// Count aligned equal-sized guests without allocating or touching guest payload pages.
    pub(super) fn capacity(&self, size: usize, align: usize) -> usize {
        let Some(stride) = size.checked_next_multiple_of(align) else {
            return 0;
        };
        let mut count = 0;
        let mut link = self.first.as_deref();
        while let Some(run) = link {
            if let Some(first) = run.base().checked_next_multiple_of(align) {
                if let Some(tail) = run
                    .end
                    .checked_sub(first)
                    .and_then(|len| len.checked_sub(size))
                {
                    count += 1 + tail / stride;
                }
            }
            link = run.next.as_deref();
        }
        count
    }
    /// Domains use low aligned addresses, permanent metadata prefers the high end
    /// of a fitting gap so it need not consume the next guest-aligned start.
    pub(super) fn take(&mut self, size: usize, align: usize, high: bool) -> Option<PhysRange> {
        let mut link = &mut self.first;
        while let Some(run) = link.take() {
            let base = run.base();
            let pos = if high {
                run.end
                    .checked_sub(size)
                    .map(|p| p & !(align - 1))
                    .filter(|&p| p >= base)
            } else {
                base.checked_next_multiple_of(align)
            };
            let span = pos
                .and_then(|p| PhysRange::new(p, size).ok())
                .filter(|r| r.end() <= run.end);
            let Some(span) = span else {
                link = &mut link.insert(run).next;
                continue;
            };
            let rest = run.next.take();
            let rest = if span.end() < run.end {
                // SAFETY: both boundaries are page-aligned; the suffix is exclusively free.
                Some(unsafe { header(span.end(), run.end, rest) })
            } else {
                rest
            };
            if base < span.base() {
                run.end = span.base();
                run.next = rest;
                *link = Some(run);
            } else {
                *link = rest;
            }
            return Some(span);
        }
        None
    }
    /// # Safety
    /// This exact allocation has retired with no remaining references, mappings or DMA.
    pub(super) unsafe fn give(&mut self, span: PhysRange) {
        let (mut lo, mut hi) = (span.base(), span.end());
        let mut link = &mut self.first;
        while let Some(run) = link.take() {
            let base = run.base();
            if run.end < lo {
                link = &mut link.insert(run).next;
                continue;
            }
            if base > hi {
                *link = Some(run);
                break;
            }
            debug_assert!(run.end == lo || base == hi, "receipt overlaps free RAM");
            lo = lo.min(base);
            hi = hi.max(run.end);
            *link = run.next.take();
        }
        let next = link.take();
        // SAFETY: the receipt and adjacent removed free runs now form one exclusive span.
        *link = Some(unsafe { header(lo, hi, next) });
    }
}
unsafe fn header(base: usize, end: usize, next: Link) -> &'static mut Run {
    let ptr = base as *mut Run;
    // SAFETY: caller owns the full first page, no old header references survive;
    // initialize the header before forming a reference, never interpret stale guest data.
    unsafe {
        ptr.write(Run { end, next });
        &mut *ptr
    }
}
