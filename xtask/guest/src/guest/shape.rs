//! Read Linux's exported boot tree and verify which CPUs can actually run PID1.
//! Ownership/Lock Ordering: one guest task, private bounded scratch, no host interfaces.
//! Invariants: emit a marker only when kernel scheduling and advertised RAM both match.
use super::sys::{self, Result};
use pauli_hal::fdt::Fdt;

pub fn check() -> Result<()> {
    sys::mount(c"sysfs", c"/sys")?;
    let fd = sys::open(c"/sys/firmware/fdt", 0)?;
    let mut bytes = [0u8; pauli_config::MAX_GUEST_DTB];
    let mut len = 0;
    while len < bytes.len() {
        let read = sys::read(fd, &mut bytes[len..])?;
        if read == 0 {
            break;
        }
        len += read;
    }
    sys::close(fd);
    let tree = Fdt::new(&bytes[..len]).map_err(|_| ())?;
    let mut memory = None;
    let mut advertised = 0u64;
    for node in tree.nodes() {
        let node = node.map_err(|_| ())?;
        match node.text("device_type").map_err(|_| ())? {
            Some("memory") => {
                sys::check(memory.is_none())?;
                memory = node.reg().map_err(|_| ())?;
            }
            Some("cpu") => {
                let id = node.u32("reg").map_err(|_| ())?.ok_or(())?;
                let bit = 1u64.checked_shl(id).ok_or(())?;
                sys::check(advertised & bit == 0)?;
                advertised |= bit;
            }
            _ => {}
        }
    }
    let (_, bytes) = memory.ok_or(())?;
    sys::check(advertised == sys::affinity()?)?;
    match (advertised, bytes) {
        (1, n) if n == 96 << 20 => sys::marker(b"[probe] shape 1 CPU 96 MiB PASS\n"),
        (255, n) if n == 128 << 20 => {
            sys::exercise_smp(8)?;
            sys::marker(b"[probe] shape 8 CPUs 128 MiB PASS\n");
        }
        _ => return Err(()),
    }
    Ok(())
}
