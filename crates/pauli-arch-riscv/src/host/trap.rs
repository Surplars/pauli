//! Native HS interrupts only; VM exits use the separately installed guest vector.
//! Ownership/Lock Ordering: IRQ callback is bounded and never enters a guest or logs under locks.
//! Invariants: trusted host stack/tp; timer masking preserves the dispatcher's compare cache.

use pauli_hal::sync::Once;
#[derive(Clone, Copy, Debug)]
pub enum Event {
    Software,
    Timer,
    External,
}
static HANDLER: Once<fn(Event)> = Once::new();
#[repr(C, align(16))]
pub struct TrapFrame {
    pub registers: [usize; 32],
    pub pc: usize,
    pub status: usize,
    pub cause: usize,
    pub value: usize,
}

pub fn set_handler(handler: fn(Event)) -> Result<(), fn(Event)> {
    HANDLER.set(handler)
}
/// # Safety
/// Native CPU, IRQs masked, valid private aligned stack and boot-installed tp.
pub unsafe fn install_vector() {
    unsafe extern "C" {
        fn pauli_native_vector();
    }
    // SAFETY: assembler defines a naturally aligned direct-mode native vector.
    unsafe {
        core::arch::asm!("csrw stvec, {}", in(reg) pauli_native_vector as *const () as usize, options(nostack));
    }
}

// SAFETY: only the native assembly vector calls this symbol with its own aligned frame.
#[unsafe(no_mangle)]
extern "C" fn pauli_native_trap(frame: &mut TrapFrame) {
    let interrupt = 1usize << 63;
    let event = match frame.cause {
        value if value == interrupt | 1 => {
            // An IPI can arrive after the last empty scan but before idle masks
            // interrupts. WFI must still see it even if no timer is armed.
            super::irq::mask_software_interrupt();
            Event::Software
        }
        value if value == interrupt | 5 => {
            super::irq::mask_timer();
            Event::Timer
        }
        value if value == interrupt | 9 => Event::External,
        2 if super::timer::recover_boot_compare(&mut frame.pc) => return,
        // A guest never reaches this vector. Any other synchronous HS exception is an
        // unrecoverable host programming fault, not a guest fault to resume.
        _ => panic!(
            "native HS trap: cause={:#x} pc={:#x} value={:#x}",
            frame.cause, frame.pc, frame.value
        ),
    };
    match HANDLER.get() {
        Some(handler) => handler(event),
        // Boot contract: install the dispatcher before enabling any native IRQ.
        None => panic!("native IRQ before dispatcher installation"),
    }
}

// Native frame operands, derived from Rust rather than numeric assembly offsets.
core::arch::global_asm!(
    include_str!("trap.S"),
    size = const core::mem::size_of::<TrapFrame>(),
    x0 = const core::mem::offset_of!(TrapFrame, registers) + 0 * core::mem::size_of::<usize>(),
    x1 = const core::mem::offset_of!(TrapFrame, registers) + 1 * core::mem::size_of::<usize>(),
    x2 = const core::mem::offset_of!(TrapFrame, registers) + 2 * core::mem::size_of::<usize>(),
    x3 = const core::mem::offset_of!(TrapFrame, registers) + 3 * core::mem::size_of::<usize>(),
    x4 = const core::mem::offset_of!(TrapFrame, registers) + 4 * core::mem::size_of::<usize>(),
    x5 = const core::mem::offset_of!(TrapFrame, registers) + 5 * core::mem::size_of::<usize>(),
    x6 = const core::mem::offset_of!(TrapFrame, registers) + 6 * core::mem::size_of::<usize>(),
    x7 = const core::mem::offset_of!(TrapFrame, registers) + 7 * core::mem::size_of::<usize>(),
    x8 = const core::mem::offset_of!(TrapFrame, registers) + 8 * core::mem::size_of::<usize>(),
    x9 = const core::mem::offset_of!(TrapFrame, registers) + 9 * core::mem::size_of::<usize>(),
    x10 = const core::mem::offset_of!(TrapFrame, registers) + 10 * core::mem::size_of::<usize>(),
    x11 = const core::mem::offset_of!(TrapFrame, registers) + 11 * core::mem::size_of::<usize>(),
    x12 = const core::mem::offset_of!(TrapFrame, registers) + 12 * core::mem::size_of::<usize>(),
    x13 = const core::mem::offset_of!(TrapFrame, registers) + 13 * core::mem::size_of::<usize>(),
    x14 = const core::mem::offset_of!(TrapFrame, registers) + 14 * core::mem::size_of::<usize>(),
    x15 = const core::mem::offset_of!(TrapFrame, registers) + 15 * core::mem::size_of::<usize>(),
    x16 = const core::mem::offset_of!(TrapFrame, registers) + 16 * core::mem::size_of::<usize>(),
    x17 = const core::mem::offset_of!(TrapFrame, registers) + 17 * core::mem::size_of::<usize>(),
    x18 = const core::mem::offset_of!(TrapFrame, registers) + 18 * core::mem::size_of::<usize>(),
    x19 = const core::mem::offset_of!(TrapFrame, registers) + 19 * core::mem::size_of::<usize>(),
    x20 = const core::mem::offset_of!(TrapFrame, registers) + 20 * core::mem::size_of::<usize>(),
    x21 = const core::mem::offset_of!(TrapFrame, registers) + 21 * core::mem::size_of::<usize>(),
    x22 = const core::mem::offset_of!(TrapFrame, registers) + 22 * core::mem::size_of::<usize>(),
    x23 = const core::mem::offset_of!(TrapFrame, registers) + 23 * core::mem::size_of::<usize>(),
    x24 = const core::mem::offset_of!(TrapFrame, registers) + 24 * core::mem::size_of::<usize>(),
    x25 = const core::mem::offset_of!(TrapFrame, registers) + 25 * core::mem::size_of::<usize>(),
    x26 = const core::mem::offset_of!(TrapFrame, registers) + 26 * core::mem::size_of::<usize>(),
    x27 = const core::mem::offset_of!(TrapFrame, registers) + 27 * core::mem::size_of::<usize>(),
    x28 = const core::mem::offset_of!(TrapFrame, registers) + 28 * core::mem::size_of::<usize>(),
    x29 = const core::mem::offset_of!(TrapFrame, registers) + 29 * core::mem::size_of::<usize>(),
    x30 = const core::mem::offset_of!(TrapFrame, registers) + 30 * core::mem::size_of::<usize>(),
    x31 = const core::mem::offset_of!(TrapFrame, registers) + 31 * core::mem::size_of::<usize>(),
    pc = const core::mem::offset_of!(TrapFrame, pc),
    status = const core::mem::offset_of!(TrapFrame, status),
    cause = const core::mem::offset_of!(TrapFrame, cause),
    value = const core::mem::offset_of!(TrapFrame, value),
);
