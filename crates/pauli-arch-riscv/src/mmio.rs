//! Trapped integer MMIO decoding. Device address policy stays with the board.
//! Hardware HTINST avoids guest-memory walks; zero HTINST fetches through
//! both stages. Neither path turns an unchecked guest VA/GPA into a host pointer.
//! Ownership/Lock Ordering: exclusive stopped vCPU; device callback owns its own synchronization.
//! Invariants: decode and address checks precede the sole callback/PC commit.
use super::vstage::ReadOnlyStage2;
use super::{VCpuContext, VmExit};

pub use pauli_hal::device::MmioAccess;

/// Expand only RV64 C.LW/LD/SW/SD and their SP forms. Floating-point, reserved
/// and other compressed encodings are not MMIO. This is only the zero-HTINST
/// fallback; transformed instructions already carry expanded register fields.
fn expand_compressed(c: u16) -> Option<u32> {
    let c = u32::from(c);
    let kind = c >> 13;
    let load = matches!(kind, 2 | 3);
    if !matches!(kind, 2 | 3 | 6 | 7) {
        return None;
    }
    let double = kind & 1 != 0;
    let (rs1, reg, imm) = match c & 3 {
        0 => (
            8 + ((c >> 7) & 7),
            8 + ((c >> 2) & 7),
            ((c >> 10) & 7) << 3
                | if double {
                    ((c >> 5) & 3) << 6
                } else {
                    ((c >> 6) & 1) << 2 | ((c >> 5) & 1) << 6
                },
        ),
        2 if load => {
            let reg = (c >> 7) & 31;
            if reg == 0 {
                return None;
            }
            (
                2,
                reg,
                ((c >> 12) & 1) << 5
                    | if double {
                        ((c >> 5) & 3) << 3 | ((c >> 2) & 7) << 6
                    } else {
                        ((c >> 4) & 7) << 2 | ((c >> 2) & 3) << 6
                    },
            )
        }
        2 => (
            2,
            (c >> 2) & 31,
            if double {
                ((c >> 10) & 7) << 3 | ((c >> 7) & 7) << 6
            } else {
                ((c >> 9) & 15) << 2 | ((c >> 7) & 3) << 6
            },
        ),
        _ => return None,
    };
    let funct = if double { 3 } else { 2 };
    Some(if load {
        (imm << 20) | (rs1 << 15) | (funct << 12) | (reg << 7) | 3
    } else {
        ((imm >> 5) << 25) | (reg << 20) | (rs1 << 15) | (funct << 12) | ((imm & 31) << 7) | 0x23
    })
}

/// Emulate only an explicit, aligned RV64 integer load/store. Unsupported or
/// implicit page-table faults fail closed, without invoking a device callback.
/// # Safety
/// Instruction backing/tables satisfy `vstage::fetch_u32`'s retained RAM and
/// foreign-storage snapshot contract. The callback must not reenter this vCPU
/// or change its immutable G-stage mapping; shared devices synchronize peers.
/// `exit` must be the trusted HS snapshot of THIS trap, not guest-provided
/// scalars: nonzero HTINST/STVAL/HTVAL attest the explicit access in hardware.
pub unsafe fn emulate(
    ctx: &mut VCpuContext,
    space: &impl ReadOnlyStage2,
    exit: VmExit,
    device: &mut dyn FnMut(MmioAccess) -> Option<usize>,
) -> bool {
    let load = exit.cause == VmExit::GUEST_LOAD_PAGE_FAULT;
    if !exit.from_guest
        || exit.pc != ctx.pc
        || exit.pc & 1 != 0
        || (!load && exit.cause != VmExit::GUEST_STORE_PAGE_FAULT)
    {
        return false;
    }
    // This narrow device path supports little-endian supervisor MMIO only.
    if ctx.exit_hstatus & ((1 << 8) | (1 << 5)) != 1 << 8 {
        return false;
    }
    let supplied = exit.instruction != 0;
    // SAFETY: the caller retains the instruction and VS-table backing.
    let Some((insn, length)) = (unsafe { instruction(ctx, space, exit) }) else {
        return false;
    };
    if insn & 0x7f != if load { 0x03 } else { 0x23 } {
        return false;
    }
    let funct = (insn >> 12) & 7;
    let width = 1usize << (funct & 3);
    if (!load && funct > 3) || (load && funct == 7) {
        return false;
    }
    let rs2 = ((insn >> 20) & 31) as usize;
    let rd = ((insn >> 7) & 31) as usize;
    let address = exit.value;
    if address & (width - 1) != 0 {
        return false;
    }
    let Some(gpa) = exit
        .gpa_shifted
        .checked_mul(4)
        .map(|gpa| gpa | (address & 3))
    else {
        return false;
    };
    // SAFETY: only the fallback walks retained guest tables; no unchecked pointer.
    if !supplied && !unsafe { explicit_address(ctx, space, insn, load, address, gpa) } {
        return false;
    }
    let Some(next) = ctx.pc.checked_add(length) else {
        return false;
    };
    let shift = 64 - width * 8;
    let store_value = (if rs2 == 0 { 0 } else { ctx.x[rs2] }) << shift >> shift;
    let access = MmioAccess {
        gpa,
        width,
        write: if load { None } else { Some(store_value) },
    };
    let Some(value) = device(access) else {
        return false;
    };
    if load && rd != 0 {
        ctx.x[rd] = if funct & 4 != 0 {
            value << shift >> shift
        } else {
            ((value << shift) as isize >> shift) as usize
        };
    }
    ctx.pc = next;
    true
}

// Decode is separate from the device commit: failure cannot change PC or GPRs.
unsafe fn instruction(
    ctx: &VCpuContext,
    space: &impl ReadOnlyStage2,
    exit: VmExit,
) -> Option<(u32, usize)> {
    if exit.instruction != 0 {
        // H v1.0: bit0=1 identifies a transformed standard/custom instruction;
        // restoring bit1 gives its expanded encoding, while bit1 itself says
        // whether the original was 16 or 32 bits. Upper XLEN bits must be zero.
        // The former rs1 is now an address OFFSET, not a guest register index.
        // A nonzero offset describes part of a misaligned access we do not
        // emulate. Implicit PTE accesses (bit0=0), custom/FP/AMO/reserved forms
        // fail before any device callback; opcode/funct3 are checked below.
        let raw = exit.instruction;
        if raw >> 32 != 0 || raw & 1 == 0 || (raw >> 15) & 31 != 0 {
            return None;
        }
        Some(((raw as u32) | 2, if raw & 2 == 0 { 2 } else { 4 }))
    } else {
        let Some((instruction, length)) =
            // SAFETY: the caller retains the executable/table RAM; the walker independently validates each GPA.
            (unsafe { super::vstage::fetch_instruction(space, ctx.vsatp, ctx.pc) })
        else {
            return None;
        };
        let insn = if length == 2 {
            expand_compressed(instruction as u16)
        } else {
            Some(instruction)
        };
        let Some(insn) = insn else { return None };
        Some((insn, length))
    }
}

unsafe fn explicit_address(
    ctx: &VCpuContext,
    space: &impl ReadOnlyStage2,
    insn: u32,
    load: bool,
    address: usize,
    gpa: usize,
) -> bool {
    let rs1 = ((insn >> 15) & 31) as usize;
    let immediate = if load {
        insn >> 20
    } else {
        ((insn >> 25) << 5) | ((insn >> 7) & 31)
    };
    let signed = ((immediate as i32) << 20 >> 20) as isize as usize;
    if (if rs1 == 0 { 0 } else { ctx.x[rs1] }).wrapping_add(signed) != address {
        return false;
    }
    // With zero HTINST, a RAM decode alone cannot distinguish an explicit
    // device access from a fault in an implicit VS page walk. Revalidate
    // its data translation. A validated transformed instruction already
    // attests an explicit access and its GPA, so rewalking would be both
    // redundant and inconsistent with a sibling's concurrent PTE update.
    let Some(inner) =
        // SAFETY: the caller retains the executable/table RAM; the walker independently validates each GPA.
        (unsafe { super::vstage::supervisor_data_gpa(space, ctx.vsatp, address, !load) })
    else {
        return false;
    };
    if inner.raw() != gpa {
        return false;
    }
    true
}
