//! RISC-V Linux syscall boundary for the static smoke fixture, not a userspace SDK.
//! Ownership/Lock Ordering: buffers are borrowed only for each synchronous call.
//! Invariants: bounded C strings and slices; negative syscall results never become sizes.
use core::{arch::asm, ffi::CStr};
pub type Result<T> = core::result::Result<T, ()>;
const AT_FDCWD: usize = (-100isize) as usize;
const OPENAT: usize = 56;
const CLOSE: usize = 57;
const READ: usize = 63;
const WRITE: usize = 64;
const MOUNT: usize = 40;
const NANOSLEEP: usize = 101;
const SET_AFFINITY: usize = 122;
const GET_AFFINITY: usize = 123;
const GETCPU: usize = 168;
const GETPID: usize = 172;
const REBOOT: usize = 142;
const IOCTL: usize = 29;
const TTY_DRAIN: usize = 0x5409; // TCSBRK(1)

/// RISC-V Linux uses the asm-generic TCGETS/TCSETS layout (not libc termios).
#[repr(C)]
#[derive(Default)]
struct Termios {
    input: u32,
    output: u32,
    control: u32,
    local: u32,
    line: u8,
    cc: [u8; 19],
}
const _: () = assert!(size_of::<Termios>() == 36);

/// # Safety
/// All pointer arguments must name live buffers of the syscall's declared size
/// and mutability. Scalar arguments must select only the fixture's own resources.
pub unsafe fn call(number: usize, [mut a0, a1, a2, a3, a4, a5]: [usize; 6]) -> isize {
    // SAFETY: the caller establishes the Linux argument and memory contract.
    unsafe {
        asm!("ecall", in("a7") number, inlateout("a0") a0, in("a1") a1,
            in("a2") a2, in("a3") a3, in("a4") a4, in("a5") a5, options(nostack));
    }
    a0 as isize
}
pub fn value(raw: isize) -> Result<usize> {
    usize::try_from(raw).map_err(|_| ())
}
pub fn check(success: bool) -> Result<()> {
    if success { Ok(()) } else { Err(()) }
}
pub fn pid() -> isize {
    // SAFETY: getpid takes no arguments.
    unsafe { call(GETPID, [0; 6]) }
}
pub fn open(path: &CStr, flags: usize) -> Result<usize> {
    // SAFETY: CStr supplies a retained, terminated path in this guest.
    value(unsafe { call(OPENAT, [AT_FDCWD, path.as_ptr() as usize, flags, 0, 0, 0]) })
}
pub fn close(fd: usize) {
    // SAFETY: scalar fixture file descriptor.
    let _ = unsafe { call(CLOSE, [fd, 0, 0, 0, 0, 0]) };
}
pub fn read(fd: usize, dest: &mut [u8]) -> Result<usize> {
    // SAFETY: kernel may write only within this uniquely borrowed slice.
    value(unsafe { call(READ, [fd, dest.as_mut_ptr() as usize, dest.len(), 0, 0, 0]) })
}
/// Only the smoke PID1 owns this tty. A command is one key, its reply is a
/// probe marker: neither a second UART byte (LF) nor terminal echo is needed.
pub fn single_key_input() -> Result<()> {
    const TCGETS: usize = 0x5401;
    const TCSETS: usize = 0x5402;
    const ICANON: u32 = 0x02;
    const ECHO: u32 = 0x08;
    const VTIME: usize = 5;
    const VMIN: usize = 6;
    let mut tty = Termios::default();
    // SAFETY: fd 0 is the probe console; TCGETS writes one live RV64 termios.
    value(unsafe { call(IOCTL, [0, TCGETS, &raw mut tty as usize, 0, 0, 0]) })?;
    tty.local &= !(ICANON | ECHO);
    tty.cc[VTIME] = 0;
    tty.cc[VMIN] = 1;
    // SAFETY: TCSETS reads that same initialized structure before returning.
    value(unsafe { call(IOCTL, [0, TCSETS, &raw const tty as usize, 0, 0, 0]) })?;
    Ok(())
}
pub fn marker(text: &[u8]) {
    // SAFETY: stdout is this Linux console and the source bytes are live throughout write().
    let _ = unsafe { call(WRITE, [1, text.as_ptr() as usize, text.len(), 0, 0, 0]) };
    // SAFETY: TCSBRK(1) drains the same tty, without an argument pointer.
    let _ = unsafe { call(IOCTL, [1, TTY_DRAIN, 1, 0, 0, 0]) };
}
pub fn sleep() -> Result<()> {
    let duration = [0usize, 20_000_000]; // struct __kernel_timespec
    // SAFETY: 64-bit timespec input; NULL remaining-time pointer.
    check(unsafe { call(NANOSLEEP, [duration.as_ptr() as usize, 0, 0, 0, 0, 0]) } == 0)
}
pub fn mount_devices() -> Result<()> {
    mount(c"devtmpfs", c"/dev")
}
pub fn mount(kind: &CStr, at: &CStr) -> Result<()> {
    // SAFETY: live C strings, no filesystem-specific data; target is in the guest.
    check(
        unsafe {
            call(
                MOUNT,
                [
                    kind.as_ptr() as usize,
                    at.as_ptr() as usize,
                    kind.as_ptr() as usize,
                    0,
                    0,
                    0,
                ],
            )
        } == 0,
    )
}
pub fn affinity() -> Result<u64> {
    let mut mask = 0u64;
    // SAFETY: Linux writes the specified eight-byte affinity mask.
    value(unsafe {
        call(
            GET_AFFINITY,
            [0, 8, &mut mask as *mut u64 as usize, 0, 0, 0],
        )
    })?;
    Ok(mask)
}
fn set_affinity(mask: u64) -> Result<()> {
    // SAFETY: Linux reads this live eight-byte mask for the calling task.
    value(unsafe { call(SET_AFFINITY, [0, 8, &raw const mask as usize, 0, 0, 0]) })?;
    Ok(())
}
pub fn exercise_smp(cpus: u32) -> Result<()> {
    let original = affinity()?;
    check(original == (1u64 << cpus) - 1)?;
    for cpu in (0..cpus).rev() {
        set_affinity(1u64 << cpu)?;
        let mut current = u32::MAX;
        // SAFETY: getcpu writes only the live CPU number; both optional pointers are null.
        value(unsafe { call(GETCPU, [&raw mut current as usize, 0, 0, 0, 0, 0]) })?;
        check(current == cpu)?;
    }
    set_affinity(original)
}
pub fn poweroff() -> ! {
    const MAGIC1: usize = 0xfee1_dead;
    const MAGIC2: usize = 672274793;
    const POWER_OFF: usize = 0x4321_fedc;
    // SAFETY: PID1 requests only its own VM-local Linux/SBI shutdown.
    let _ = unsafe { call(REBOOT, [MAGIC1, MAGIC2, POWER_OFF, 0, 0, 0]) };
    loop {
        core::hint::spin_loop();
    }
}
