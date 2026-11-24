//! Translate a static identity-linked VS ELF to a private cold-boot template.
//! Ownership/Lock Ordering: host-only immutable input; no guest or device locks.
//! Invariants: RX -> optional R -> RW; BSS consumes RAM but not stored image bytes.
use super::{flatten, read, segments};
use anyhow::{Result, ensure};
use pauli_config::{FIRMWARE_PAGE, FirmwareLayout, MAX_FIRMWARE_RAM};
#[cfg(test)]
pub(crate) mod tests;

pub fn load(data: &[u8], entry: u64) -> Result<(Vec<u8>, u64, FirmwareLayout)> {
    ensure!(
        u16::from_le_bytes(read(data, 16)?) == 2,
        "firmware must be ET_EXEC"
    );
    const EF_RISCV_RVC: u32 = 1;
    ensure!(
        u32::from_le_bytes(read(data, 48)?) & !EF_RISCV_RVC == 0,
        "firmware requires the integer RV64 soft-float ABI"
    );
    let (layout, mut parts) = segments(data, entry, MAX_FIRMWARE_RAM)?;
    parts.retain(|p| p.memory != 0);
    parts.sort_unstable_by_key(|p| p.address);
    ensure!(
        parts
            .first()
            .is_some_and(|p| p.address == entry && p.flags == 5 && p.bytes != 0),
        "firmware entry must start an RX segment"
    );
    let mut previous = 0;
    let mut previous_end = 0;
    let mut ends = [0u64; 3];
    for p in &parts {
        let rank = match p.flags {
            5 => 0, // RX
            4 => 1, // R
            6 => 2, // RW
            _ => anyhow::bail!("unsupported firmware segment permissions"),
        };
        let start = p.address - entry;
        ensure!(
            p.vaddr == p.address && start >= previous_end && rank >= previous,
            "firmware segments must be identity-linked, nonoverlapping RX/R/RW"
        );
        ensure!(
            rank == previous || start.is_multiple_of(FIRMWARE_PAGE),
            "firmware permissions change inside a page"
        );
        previous_end = start + p.memory; // segments() checked the relative extent.
        ends[rank] = previous_end;
        previous = rank;
    }
    let rx = ends[0].div_ceil(FIRMWARE_PAGE);
    let ro = ends[1].max(ends[0]).div_ceil(FIRMWARE_PAGE) - rx;
    Ok((
        flatten(data, &layout, parts),
        layout.ram_span,
        FirmwareLayout {
            rx_pages: rx.try_into()?,
            ro_pages: ro.try_into()?,
        },
    ))
}
