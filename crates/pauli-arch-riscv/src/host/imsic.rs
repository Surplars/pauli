//! Calling CPU's native S-file operations; virtual guest files use a separate bank.
//! Ownership/Lock Ordering: mask local IRQs around indirect selector/data pairs.
//! Invariants: file zero belongs to HS; reset is explicit boot ownership, never guest reset.

use super::{aia_csr::supervisor as csr, irq::RiscvIrq};
use pauli_hal::{
    device::{DeviceError, Result},
    sync::LocalIrq,
};
const DELIVERY: usize = 0x70;
const THRESHOLD: usize = 0x72;
const PENDING: usize = 0x80;
const ENABLE: usize = 0xc0;
const XLEN: usize = 64;
pub const MAX_ID: usize = 2047;

/// # Safety
/// Ssaia/IMSIC is firmware-discovered and this CPU exclusively owns its HS file.
pub unsafe fn init(ids: usize) -> Result<()> {
    if ids < 63 || ids > MAX_ID || ids % 64 != 63 {
        return Err(DeviceError::InvalidResource);
    }
    let saved = RiscvIrq::save();
    // SAFETY: exclusive native file, masked selector/data pairs and supported selectors.
    unsafe {
        write(DELIVERY, 0);
        write(THRESHOLD, 0);
        for word in 0..=(ids / XLEN) {
            write(ENABLE + word * 2, 0);
            write(PENDING + word * 2, 0);
        }
        write(DELIVERY, 1);
        RiscvIrq::restore(saved);
    }
    Ok(())
}
/// # Safety
/// Calling CPU owns its discovered HS IMSIC file; identity is implemented by that file.
pub unsafe fn set_enabled(identity: usize, enabled: bool) {
    let saved = RiscvIrq::save();
    let select = ENABLE + identity / XLEN * 2;
    let bit = 1usize << (identity % XLEN);
    // SAFETY: caller owns the native file and identity; retain firmware's prior selector.
    unsafe {
        let old_select = csr::siselect::read();
        csr::siselect::write(select);
        let before = csr::sireg::read();
        csr::sireg::write(if enabled { before | bit } else { before & !bit });
        csr::siselect::write(old_select);
        RiscvIrq::restore(saved);
    }
}
/// # Safety
/// Native IRQ owner on the current Ssaia CPU, not an arbitrary guest CSR request.
pub unsafe fn claim() -> u32 {
    // SAFETY: this CPU owns S-file zero; CSRRW atomically claims one identity.
    (unsafe { csr::stopei::claim() } & MAX_ID) as u32
}
unsafe fn write(select: usize, value: usize) {
    // SAFETY: exclusive native file, IRQs masked by the caller; selectors are protocol constants.
    unsafe {
        let old = csr::siselect::read();
        csr::siselect::write(select);
        csr::sireg::write(value);
        csr::siselect::write(old);
    }
}
