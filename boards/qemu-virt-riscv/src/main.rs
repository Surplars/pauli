#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]
//! OpenSBI QEMU virt payload; discover and publish native resources, then enter pauli-core.
//! Ownership/Lock Ordering: one boot owner initializes; board handles no VM lifecycle.
//! Invariants: loader/firmware/native DMA are reserved before guest assignments run.
#[cfg(target_os = "none")]
mod board;
#[cfg(target_os = "none")]
mod devices;
#[cfg(target_os = "none")]
use qemu_virt_riscv::fdt;
#[cfg(target_os = "none")]
mod irq;
#[cfg(target_os = "none")]
mod loader;

#[cfg(not(target_os = "none"))]
fn main() {}

#[cfg(target_os = "none")]
core::arch::global_asm!(include_str!("../startup.S"), sie = const pauli_arch_riscv::bits::SSTATUS_SIE);

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn pauli_boot(hart: usize, dtb: usize) -> ! {
    let _ = pauli_hal::log::install(&devices::BootConsole);
    match board::prepare(hart, dtb) {
        Ok(board) => pauli_core::boot(board, &board::RUNTIME),
        Err(error) => {
            pauli_hal::hv_error!("QEMU virt boot: {error:?}");
            board::poweroff()
        }
    }
}
#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    pauli_hal::hv_error!("native HS failure: {info}");
    board::poweroff()
}
