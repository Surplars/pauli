//! Exact-width foreign-memory snapshots, independent of native DMA.
//! Ownership/Lock Ordering: caller validates bounds and owns the CPU-copy operation.
//! Invariants: never form guest slices; aligned 2/4-byte fields retain their width.
/// # Safety
/// The checked host interval is retained, readable foreign RAM, disjoint
/// from the output slice for its full length.
pub(super) unsafe fn read(mut host: usize, mut output: &mut [u8]) {
    while !output.is_empty() {
        let width = width(host, output.len());
        // SAFETY: caller checked the entire range; chosen width is naturally aligned.
        unsafe {
            match width {
                8 => output
                    .as_mut_ptr()
                    .cast::<u64>()
                    .write_unaligned((host as *const u64).read_volatile()),
                4 => output
                    .as_mut_ptr()
                    .cast::<u32>()
                    .write_unaligned((host as *const u32).read_volatile()),
                2 => output
                    .as_mut_ptr()
                    .cast::<u16>()
                    .write_unaligned((host as *const u16).read_volatile()),
                _ => output[0] = (host as *const u8).read_volatile(),
            }
        }
        host += width;
        output = &mut output[width..];
    }
}
/// # Safety
/// The checked host interval is retained, writable foreign RAM, disjoint
/// from input. The caller owns the host-side write operation.
pub(super) unsafe fn write(mut host: usize, mut input: &[u8]) {
    while !input.is_empty() {
        let width = width(host, input.len());
        // SAFETY: caller checked the range; host is aligned, ordinary input may be unaligned.
        unsafe {
            match width {
                8 => {
                    (host as *mut u64).write_volatile(input.as_ptr().cast::<u64>().read_unaligned())
                }
                4 => {
                    (host as *mut u32).write_volatile(input.as_ptr().cast::<u32>().read_unaligned())
                }
                2 => {
                    (host as *mut u16).write_volatile(input.as_ptr().cast::<u16>().read_unaligned())
                }
                _ => (host as *mut u8).write_volatile(input[0]),
            }
        }
        host += width;
        input = &input[width..];
    }
}
fn width(host: usize, len: usize) -> usize {
    if host & 7 == 0 && len >= 8 {
        8
    } else if host & 3 == 0 && len >= 4 {
        4
    } else if host & 1 == 0 && len >= 2 {
        2
    } else {
        1
    }
}
