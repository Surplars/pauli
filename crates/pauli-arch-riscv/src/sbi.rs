//! Guest SBI v0.3: BASE/TIME/SRST plus opt-in VM-local HSM/IPI/RFENCE.
//! Never forward guest reset/HSM/IPI/RFENCE requests to host OpenSBI. An
//! unsupported extension/function is reported honestly and PC advances once.
//! Reference: riscv-non-isa/riscv-sbi-doc, ext-base/ext-time/ext-sys-reset.adoc.
//! Ownership/Lock Ordering: exclusive stopped vCPU; deferred actions are committed by the VM owner.
//! Invariants: guest HSM/RFENCE/SRST never call physical firmware.
use super::{VCpuContext, VmExit};
use crate::VmResetKind;
#[cfg(feature = "guest-control")]
use pauli_core::arch::MANAGEMENT_EID;

pub const BASE: usize = 0x10;
pub const SRST: usize = 0x5352_5354;
pub const TIME: usize = 0x5449_4d45;
pub const HSM: usize = 0x4853_4d;
pub const IPI: usize = 0x7350_49;
pub const RFENCE: usize = 0x5246_4e43;
pub const FAILED: isize = -1;
pub const NOT_SUPPORTED: isize = -2;
pub const INVALID_PARAM: isize = -3;
pub const INVALID_ADDRESS: isize = -5;
pub const ALREADY_AVAILABLE: isize = -6;
// Private experimental identity, not a claim of a registered SBI impl ID.
const IMPLEMENTATION_ID: usize = 0x4f49_4f4e;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SbiAction {
    Resume,
    SetTimer {
        deadline: u64,
    },
    Shutdown {
        reason: u32,
    },
    Reset {
        kind: VmResetKind,
        reason: u32,
    },
    HartStart {
        hart: usize,
        entry: usize,
        opaque: usize,
    },
    HartStop,
    HartStatus {
        hart: usize,
    },
    SendIpi {
        mask: usize,
        base: usize,
    },
    RemoteFence {
        mask: usize,
        base: usize,
        instruction: bool,
    },
    #[cfg(feature = "guest-control")]
    ManagementProbe,
    #[cfg(feature = "guest-control")]
    Management {
        args: [u64; 6],
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SbiError {
    NotVsEcall,
    InvalidPc,
}

/// Only a real VS ecall exit may invoke this service. The caller retains CPU
/// ownership until it applies the action and commits the VM lifecycle. Successful
/// shutdown/reset never returns to the guest, so PC/results stay intact.
pub fn handle_ecall(ctx: &mut VCpuContext, exit: VmExit) -> Result<SbiAction, SbiError> {
    handle_ecall_mode(ctx, exit, false)
}

/// SMP requests are deferred to the VM owner, without consuming PC/results.
/// It validates/applies the request against that VM's vCPUs, then completes the
/// call once. Host firmware is never a target for guest HSM/IPI/RFENCE.
pub fn handle_ecall_mode(
    ctx: &mut VCpuContext,
    exit: VmExit,
    smp: bool,
) -> Result<SbiAction, SbiError> {
    if !exit.from_guest || exit.cause != VmExit::VS_ECALL || exit.pc != ctx.pc {
        return Err(SbiError::NotVsEcall);
    }
    let eid = ctx.x[17];
    let fid = ctx.x[16];
    let mut action = SbiAction::Resume;
    let next_pc = ctx.pc.checked_add(4).ok_or(SbiError::InvalidPc)?;
    #[cfg(feature = "guest-control")]
    if eid == MANAGEMENT_EID as usize && fid == 1 {
        // Owner transport authenticates the source and decodes these scalars.
        // Keep PC/results intact until its bounded request is completed.
        return Ok(SbiAction::Management {
            args: core::array::from_fn(|index| ctx.x[10 + index] as u64),
        });
    }
    if smp {
        if let Some(action) = deferred(ctx, eid, fid) {
            return Ok(action);
        }
    }
    let (error, value) = match (eid, fid) {
        #[cfg(feature = "guest-control")]
        (eid, 0 | 2) if eid == MANAGEMENT_EID as usize => {
            if ctx.x[10..16].iter().any(|value| *value != 0) {
                (INVALID_PARAM, 0)
            } else if fid == 0 {
                return Ok(SbiAction::ManagementProbe);
            } else {
                (0, pauli_core::arch::FEAT_STATS as usize)
            }
        }
        (BASE, 0) => (0, 3), // ABI v0.3; only advertised optional EIDs exist.
        (BASE, 1) => (0, IMPLEMENTATION_ID),
        (BASE, 2) => (0, 1), // OpenIon experimental implementation revision.
        (BASE, 3) => (0, usize::from(advertises(ctx.x[10], smp))),
        (BASE, 4..=6) => (0, 0), // Zero is valid for guest machine identities.
        (TIME, 0) => {
            action = SbiAction::SetTimer {
                deadline: ctx.x[10] as u64,
            };
            (0, 0)
        }
        (HSM, 1) if smp => (FAILED, 0), // Caller must disable VS interrupts.
        (SRST, 0) => match reset(ctx.x[10] as u32, ctx.x[11] as u32) {
            Some(action) => return Ok(action),
            None => (INVALID_PARAM, 0),
        },
        _ => (NOT_SUPPORTED, 0),
    };
    ctx.x[10] = error as usize;
    ctx.x[11] = value;
    ctx.pc = next_pc;
    Ok(action)
}

fn advertises(eid: usize, smp: bool) -> bool {
    #[cfg(feature = "guest-control")]
    if eid == MANAGEMENT_EID as usize {
        return true;
    }
    matches!(eid, BASE | SRST | TIME) || (smp && matches!(eid, HSM | IPI | RFENCE))
}

fn deferred(ctx: &VCpuContext, eid: usize, fid: usize) -> Option<SbiAction> {
    match (eid, fid) {
        (HSM, 0) => Some(SbiAction::HartStart {
            hart: ctx.x[10],
            entry: ctx.x[11],
            opaque: ctx.x[12],
        }),
        (HSM, 1) if ctx.vsstatus & crate::bits::SSTATUS_SIE == 0 => Some(SbiAction::HartStop),
        (HSM, 2) => Some(SbiAction::HartStatus { hart: ctx.x[10] }),
        (IPI, 0) => Some(SbiAction::SendIpi {
            mask: ctx.x[10],
            base: ctx.x[11],
        }),
        // The serialized backend safely over-invalidates the whole guest
        // address space for SFENCE.VMA/ASID ranges; it never dereferences
        // an untrusted range. Nested hypervisor fences stay unsupported.
        (RFENCE, 0..=2) => Some(SbiAction::RemoteFence {
            mask: ctx.x[10],
            base: ctx.x[11],
            instruction: fid == 0,
        }),
        _ => None,
    }
}

// SBI SRST arguments are u32; the caller intentionally truncates upper XLEN bits.
fn reset(kind: u32, reason: u32) -> Option<SbiAction> {
    if kind > 2 || reason > 1 {
        return None;
    }
    Some(if kind == 0 {
        SbiAction::Shutdown { reason }
    } else {
        SbiAction::Reset {
            kind: if kind == 1 {
                VmResetKind::Cold
            } else {
                VmResetKind::Warm
            },
            reason,
        }
    })
}

/// Decode all targets before any side effect. Base=-1 means every configured
/// virtual hart; otherwise every set bit must name an in-VM hart, even when a
/// later invalid bit follows valid ones. No host hart mask is ever produced.
pub fn hart_mask(mask: usize, base: usize, count: usize) -> Result<usize, isize> {
    pauli_core::arch::cpu_targets(mask, base, count).ok_or(INVALID_PARAM)
}
