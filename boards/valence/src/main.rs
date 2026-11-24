#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]
//! Valence OpenSBI payload; no physical disk, network or RTC.
//! Ownership/Lock Ordering: single boot hart and static board wiring; core owns VMs.
//! Invariants: use only delegated S-APLIC/S-file, no IMSIC CPU-MMIO access.
#[cfg(any(target_os = "none", test))]
mod assignment;
#[cfg(target_os = "none")]
mod board;
#[cfg(any(target_os = "none", test))]
mod external;
#[cfg(target_os = "none")]
mod guest;
#[cfg(any(target_os = "none", test))]
mod handoff;
#[cfg(not(target_os = "none"))]
fn main() {}

#[cfg(target_os = "none")]
core::arch::global_asm!(include_str!("../startup.S"), sie = const pauli_arch_riscv::bits::SSTATUS_SIE);

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn pauli_boot(hart: usize, dtb: usize) -> ! {
    let _ = pauli_hal::log::install(&board::BootConsole);
    // SAFETY: this is the sole OpenSBI boot hart; a1 denotes the retained native FDT.
    match unsafe { board::prepare(hart, dtb) } {
        Ok(board) => pauli_core::boot(board, &board::RUNTIME),
        Err(error) => {
            pauli_hal::hv_error!("Wave8 boot: {error:?}");
            board::halt()
        }
    }
}
#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    pauli_hal::hv_error!("Wave8 native HS failure: {info}");
    board::halt()
}
