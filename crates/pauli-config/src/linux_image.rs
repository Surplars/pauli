//! Raw RISC-V Linux Image contract shared by the package reader and the board.
//! Pure header inspection: neither the source nor guest RAM is mutated. The file
//! extent is retained for copies; the larger in-memory extent also reserves BSS.
use crate::{Error, KERNEL_OFFSET, MAX_KERNEL_BYTES};

pub const HEADER_BYTES: usize = 64;

/// `header` may be only the initial 64 bytes, `file_size` is the full staged file.
/// Guest geometry separately checks the returned extent against initrd and DTB.
pub fn memory_size(header: &[u8], file_size: u64) -> Result<u64, Error> {
    let bytes = header.first_chunk::<HEADER_BYTES>().ok_or(Error::Image)?;
    let word = |at: usize| u64::from_le_bytes(core::array::from_fn(|i| bytes[at + i]));
    let loaded = word(16);
    if &bytes[56..60] != b"RSC\x05"
        || word(8) != KERNEL_OFFSET
        || word(24) & 1 != 0 // Big-endian guest.
        || !(HEADER_BYTES as u64..=MAX_KERNEL_BYTES).contains(&file_size)
        || loaded < file_size
    {
        return Err(Error::Image);
    }
    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_header_separately_from_the_file_and_bss_extents() {
        let mut h = [0; HEADER_BYTES];
        h[56..60].copy_from_slice(b"RSC\x05");
        h[8..16].copy_from_slice(&KERNEL_OFFSET.to_le_bytes());
        h[16..24].copy_from_slice(&8192u64.to_le_bytes());
        assert_eq!(memory_size(&h, 4096), Ok(8192));
        for size in [0, 63, 8193, MAX_KERNEL_BYTES + 1] {
            assert_eq!(memory_size(&h, size), Err(Error::Image));
        }
        assert_eq!(memory_size(&h[..63], 4096), Err(Error::Image));
        for at in [8, 24, 56] {
            let mut bad = h;
            bad[at] ^= 1;
            assert_eq!(memory_size(&bad, 4096), Err(Error::Image));
        }
    }
}
