//! Native HS-to-firmware SBI calls, never a guest request forwarding layer.
//! Ownership/Lock Ordering: caller owns timer/hart/console policy; no locks.
//! Invariants: only validated physical owners are targets; firmware results propagate.

pub const BASE: usize = 0x10;
pub const TIME: usize = 0x5449_4d45;
pub const HSM: usize = 0x4853_4d;
pub const IPI: usize = 0x7350_49;
pub const DBCN: usize = 0x4442_434e;
pub const SRST: usize = 0x5352_5354;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SbiRet {
    pub error: isize,
    pub value: usize,
}
fn call(eid: usize, fid: usize, args: [usize; 6]) -> SbiRet {
    let (error, value);
    // SAFETY: HS firmware ABI preserves all non-result registers. The caller
    // owns any physical buffers passed to supported native SBI functions.
    unsafe {
        core::arch::asm!("ecall", inlateout("a0") args[0] => error, inlateout("a1") args[1] => value, in("a2") args[2], in("a3") args[3], in("a4") args[4], in("a5") args[5], in("a6") fid, in("a7") eid, options(nostack));
    }
    SbiRet { error, value }
}
pub fn probe(eid: usize) -> bool {
    let ret = call(BASE, 3, [eid, 0, 0, 0, 0, 0]);
    ret.error == 0 && ret.value != 0
}
pub fn try_set_timer(deadline: u64) -> SbiRet {
    call(TIME, 0, [deadline as usize, 0, 0, 0, 0, 0])
}
/// # Safety
/// The stopped hart is exclusively assigned; entry/opaque/stack are valid,
/// identity-addressed and retained for its complete asynchronous lifetime.
pub unsafe fn hart_start(hart: usize, start: usize, opaque: usize) -> SbiRet {
    call(HSM, 0, [hart, start, opaque, 0, 0, 0])
}
pub fn hart_stop() -> SbiRet {
    call(HSM, 1, [0; 6])
}
pub fn hart_status(hart: usize) -> SbiRet {
    call(HSM, 2, [hart, 0, 0, 0, 0, 0])
}
pub fn send_ipi(hart: usize) -> SbiRet {
    // SAFETY: order published RAM work before firmware's interrupt/MMIO output.
    unsafe {
        core::arch::asm!("fence w, ow", options(nostack));
    }
    call(IPI, 0, [1, hart, 0, 0, 0, 0])
}
pub fn console_putchar(byte: u8) -> SbiRet {
    call(DBCN, 2, [byte as usize, 0, 0, 0, 0, 0])
}
pub fn shutdown() -> SbiRet {
    call(SRST, 0, [0; 6])
}

pub struct FirmwareConsole;
impl pauli_hal::log::Console for FirmwareConsole {
    fn write(&self, bytes: &[u8]) -> core::fmt::Result {
        for &byte in bytes {
            if console_putchar(byte).error != 0 {
                return Err(core::fmt::Error);
            }
        }
        Ok(())
    }
}
