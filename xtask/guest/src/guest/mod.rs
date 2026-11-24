//! Static Linux PID1 with single-key commands and explicit success markers.
//! Ownership/Lock Ordering: single userspace task; kernel owns all syscall effects.
//! Invariants: smoke observes VU syscalls, the TIME clock and real Linux devices.
mod irq_route;
mod net;
mod shape;
mod storage;
mod sys;
use core::arch::{asm, global_asm};
global_asm!(include_str!("start.S"));
unsafe extern "C" {
    fn fp_sleep() -> usize;
}

#[unsafe(no_mangle)]
extern "C" fn probe_entry() -> ! {
    if run().is_ok() {
        sys::marker(b"[probe] Poweroff\n");
        sys::poweroff();
    }
    fail()
}
fn run() -> sys::Result<()> {
    sys::check(sys::pid() == 1)?;
    let mode = mode()?;
    let before = time();
    // SAFETY: Linux enables D for this RV64GC fixture; the leaf preserves ABI state.
    sys::check(unsafe { fp_sleep() } == 0 && time() > before)?;
    sys::marker(b"[probe] PID1 TIME FD PASS\n");
    if matches!(mode, b'S' | b'R') {
        sys::exercise_smp(2)?;
        sys::marker(b"[probe] SMP HSM affinity PASS\n");
    }
    if mode == b'R' {
        irq_route::check()?;
    }
    if mode == b'V' {
        shape::check()?;
    }
    if matches!(mode, b'I' | b'D' | b'E') {
        sys::mount_devices()?;
    }
    if matches!(mode, b'A' | b'B') {
        net::exchange(mode)?;
        sys::marker(b"[probe] NET IPv4 UDP PASS\n");
    }
    sys::single_key_input()?;
    sys::marker(b"[probe] ready\n");
    loop {
        let mut key = [0];
        sys::check(sys::read(0, &mut key)? == 1)?;
        match key[0] {
            b'p' => sys::marker(b"[probe] pong\n"),
            b'q' => return Ok(()),
            b'f' if matches!(mode, b'I' | b'D' | b'E') => return storage::private_fault(),
            b'w' | b'r' | b'z' if matches!(mode, b'D' | b'E') => storage::sector(mode, key[0])?,
            _ => {}
        }
    }
}
fn mode() -> sys::Result<u8> {
    let fd = sys::open(c"/etc/pauli-probe", 0)?;
    let mut mode = [0];
    let n = sys::read(fd, &mut mode)?;
    sys::close(fd);
    sys::check(
        n == 1
            && matches!(
                mode[0],
                b'L' | b'S' | b'R' | b'I' | b'A' | b'B' | b'D' | b'E' | b'V'
            ),
    )?;
    Ok(mode[0])
}
fn time() -> usize {
    let ticks;
    // SAFETY: Linux exposes TIME to this userspace task; no memory or stack access.
    unsafe { asm!("rdtime {}", out(reg) ticks, options(nomem, nostack)) };
    ticks
}
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    fail()
}
fn fail() -> ! {
    sys::marker(b"[probe] FAIL\n");
    sys::poweroff();
}
