//! Direct sector I/O, guest flush and an unmapped-page isolation fixture.
//! Ownership/Lock Ordering: one Linux task owns the aligned sector and fd.
//! Invariants: smoke launches only new raw disks; the fault GPA has no mapping.
use super::sys::{self, Result, call, check};
const SECTOR: usize = 512;
const O_RDWR_DIRECT: usize = 2 | 0x4000;
const PREAD64: usize = 67;
const PWRITE64: usize = 68;
const FSYNC: usize = 82;
const MMAP: usize = 222;
const PROT_READ: usize = 1;
const MAP_SHARED: usize = 1;
const TEST_HOLE: usize = 0x1100_0000;
const PAGE: usize = 4096;
#[repr(align(4096))]
struct Aligned([u8; SECTOR]);

pub fn sector(mode: u8, command: u8) -> Result<()> {
    let fd = sys::open(c"/dev/vda", O_RDWR_DIRECT)?;
    let pattern = if command == b'z' { 0 } else { mode };
    let mut data = Aligned([0; SECTOR]);
    if command == b'w' {
        data.0.fill(pattern);
        // SAFETY: aligned live input, bounded by the new smoke disk's first sector.
        check(
            unsafe { call(PWRITE64, [fd, data.0.as_ptr() as usize, SECTOR, 0, 0, 0]) }
                == SECTOR as isize,
        )?;
        // SAFETY: flush this fixture's own fd; no pointer arguments.
        check(unsafe { call(FSYNC, [fd, 0, 0, 0, 0, 0]) } == 0)?;
        sys::marker(b"[probe] disk WRITE FLUSH PASS\n");
    } else {
        // SAFETY: aligned exclusive output, bounded by the slice and disk sector.
        check(
            unsafe { call(PREAD64, [fd, data.0.as_mut_ptr() as usize, SECTOR, 0, 0, 0]) }
                == SECTOR as isize,
        )?;
        check(data.0.iter().all(|&byte| byte == pattern))?;
        sys::marker(if command == b'z' {
            b"[probe] disk ZERO PASS\n"
        } else {
            b"[probe] disk READ PASS\n"
        });
    }
    sys::close(fd);
    Ok(())
}

pub fn private_fault() -> Result<()> {
    let fd = sys::open(c"/dev/mem", 0)?;
    // SAFETY: request one read-only Linux userspace page at a deliberately
    // unmapped *guest* physical address, outside every device and RAM range.
    let mapping =
        sys::value(unsafe { call(MMAP, [0, PAGE, PROT_READ, MAP_SHARED, fd, TEST_HOLE]) })?;
    sys::marker(b"[probe] touching unmapped guest page\n");
    // SAFETY: Linux supplied an aligned userspace mapping. An actual load,
    // with no Rust reference to the unmapped backing, must exit to the VMM.
    unsafe { core::arch::asm!("ld zero, 0({})", in(reg) mapping, options(readonly, nostack)) };
    Err(())
}
