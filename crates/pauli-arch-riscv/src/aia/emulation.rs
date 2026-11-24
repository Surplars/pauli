//! Narrow VS-only AIA CSR decoder. A rejected instruction/callback changes
//! neither registers nor PC. CSRRW/RS/RC side-effect rules include rs1=x0,
//! rd=x0 and zero-valued nonzero source registers.
//! Ownership/Lock Ordering: exclusive stopped vCPU; callback takes only its VM file-bank lock.
//! Invariants: rejected CSR instructions cannot change PC or destination state.
use super::registers::{SIREG, STOPEI};
#[derive(Clone, Copy)]
pub struct CsrAccess {
    pub register: u16,
    pub select: usize,
    pub clear: usize,
    pub set: usize,
    pub write: bool,
}
pub fn decode(
    instruction: u32,
    registers: &[usize; 32],
    select: usize,
) -> Option<(CsrAccess, usize)> {
    let register = (instruction >> 20) as u16;
    if instruction & 0x7f != 0x73 || !matches!(register, SIREG | STOPEI) {
        return None;
    }
    let funct = (instruction >> 12) & 7;
    let source = ((instruction >> 15) & 31) as usize;
    let value = if funct & 4 != 0 {
        source
    } else if source == 0 {
        0
    } else {
        registers[source]
    };
    let (clear, set, write) = match funct {
        1 | 5 => (usize::MAX, value, true),
        2 | 6 => (value, value, source != 0),
        3 | 7 => (value, 0, source != 0),
        _ => return None,
    };
    Some((
        CsrAccess {
            register,
            select,
            clear,
            set,
            write,
        },
        ((instruction >> 7) & 31) as usize,
    ))
}

/// # Safety
/// The trusted trap snapshot belongs to ctx. If STVAL is zero, instruction
/// tables/backing satisfy vstage::fetch_u32's retained foreign-RAM contract.
#[cfg(target_arch = "riscv64")]
pub unsafe fn emulate(
    ctx: &mut super::super::VCpuContext,
    space: &super::super::GStagePageTable,
    exit: super::super::VmExit,
    io: &mut dyn super::super::vcpu::GuestIo,
) -> bool {
    use super::super::{VmExit, vstage};
    if !ctx.virtual_aia
        || !exit.from_guest
        || exit.cause != VmExit::VIRTUAL_INSTRUCTION
        || ctx.exit_hstatus & (1 << 8) == 0
        || exit.pc != ctx.pc
        || ctx.pc & 1 != 0
        || ctx.pc.checked_add(4).is_none()
    {
        return false;
    }
    let instruction = if exit.value != 0 {
        let Ok(value) = u32::try_from(exit.value) else {
            return false;
        };
        value
    } else {
        if ctx.exit_hstatus & (1 << 5) != 0 {
            return false;
        }
        // SAFETY: the caller retains instruction/table RAM; the fetch validates both translation stages.
        let Some(value) = (unsafe { vstage::fetch_u32(space, ctx.vsatp, ctx.pc) }) else {
            return false;
        };
        value
    };
    let Some((access, destination)) = decode(instruction, &ctx.x, ctx.aia.select) else {
        return false;
    };
    let Some(value) = io.csr(access) else {
        return false;
    };
    if destination != 0 {
        ctx.x[destination] = value;
    }
    ctx.pc += 4;
    true
}
