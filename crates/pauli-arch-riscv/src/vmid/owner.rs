//! Per-physical-owner VMID working set. Tag zero belongs to HS/unmanaged paths;
//! four cached tags cover the current VM capacity. Reuse is invalidated locally
//! before entry, not globally allocated or broadcast on every world switch.
//! Ownership/Lock Ordering: one IRQ-masked CPU accesses its own cache; no borrow spans entry.
//! Invariants: VMID zero stays HS-only; every resident owner retires before RAM reuse.
use crate::{VCpuContext, remote::FenceBatch};
use core::cell::UnsafeCell;

use super::policy::{self, Cache, Key};
const SHIFT: usize = 44;
const MASK: usize = 0x3fff << SHIFT;
const _: () = assert!(policy::TAGGED == 1 && policy::INVALIDATE == 2 && policy::VS_INVALIDATE == 4);

struct Owner {
    initialized: bool,
    cache: Cache,
}
struct Local(UnsafeCell<Owner>);
// Each element is used only by its physical hart, with IRQs masked and no
// migration/reentrant guest execution. No reference survives a guest entry.
// SAFETY: this IRQ-masked native owner retains the validated root and restores HGATP/VSATP before return.
unsafe impl Sync for Local {}
static OWNERS: [Local; crate::MAX_CPUS] = [const {
    Local(UnsafeCell::new(Owner {
        initialized: false,
        cache: Cache::new(0),
    }))
}; crate::MAX_CPUS];

unsafe fn local() -> &'static mut Owner {
    let index = crate::host::cpu_id() as usize;
    // SAFETY: boot assigns a bounded native CPU ID, and the caller excludes IRQ/reentry.
    unsafe { &mut *OWNERS[index].0.get() }
}
unsafe fn owner(root: usize) -> Result<&'static mut Owner, crate::Error> {
    // SAFETY: the caller owns the IRQ-masked physical CPU through this short borrow.
    let owner = unsafe { local() };
    if !owner.initialized {
        // Probe with a valid Sv39x4 root, rather than assuming VMID is writable
        // in Bare. Never allow a nonzero VSATP to straddle a HGATP change.
        let (old, read): (usize, usize);
        // SAFETY: this IRQ-masked native owner retains the validated root and restores HGATP/VSATP before return.
        unsafe {
            core::arch::asm!(
                ".option push", ".option arch, +h",
                "csrr {old}, hgatp", "csrr {vsatp}, vsatp",
                "csrw vsatp, zero", "csrw hgatp, {probe}", "csrr {read}, hgatp",
                "csrw hgatp, {old}", "csrw vsatp, {vsatp}",
                "hfence.gvma zero, zero", "hfence.vvma zero, zero", ".option pop",
                old = out(reg) old, vsatp = out(reg) _, read = out(reg) read,
                probe = in(reg) (root | MASK), options(nostack)
            );
        }
        if old & MASK != 0 {
            return Err(crate::Error::InvalidState);
        }
        if read & !MASK != root {
            return Err(crate::Error::Unsupported);
        }
        owner.cache = Cache::new((read & MASK) >> SHIFT);
        owner.initialized = true;
    }
    Ok(owner)
}

/// # Safety
/// HS physical owner, IRQs masked, no migration. The epoch/root is immutable
/// for its lifetime; a new VM gets a fresh kernel ID, including cold restart.
pub(crate) unsafe fn prepare(ctx: &mut VCpuContext) -> Result<(), crate::Error> {
    let root = ctx.hgatp & !MASK;
    if ctx.tlb_epoch == 0 {
        // Raw fixtures have no lifetime token. Keep their full-flush contract.
        ctx.hgatp = root;
        ctx.tlb_flags = 0;
        return Ok(());
    }
    // SAFETY: prepare's contract grants the exclusive physical owner.
    let owner = unsafe { owner(root) }?;
    let plan = owner.cache.select(
        Key {
            epoch: ctx.tlb_epoch,
            root,
        },
        ctx.tlb_vcpu,
        ctx.tlb_flags == 0,
    );
    ctx.hgatp = root | (usize::from(plan.tag) << SHIFT);
    ctx.tlb_flags = plan.flags();
    Ok(())
}

/// Fence an off-CPU target on THIS physical owner, not the currently restored
/// HS VMID. A missing binding has no surviving target translations: either it
/// never entered here or reuse already invalidated its old tag. Do not create
/// a binding or mark it current merely to service a fence request.
/// Return actual operations for diagnostics: an absent tag already satisfies
/// translation, but never satisfies a pending instruction synchronization.
/// # Safety
/// Same physical ownership/epoch contract as prepare. Complete the remote ACK
/// only after this function returns; no lock or VS execution spans the call.
pub(crate) unsafe fn synchronize(ctx: &VCpuContext, batch: FenceBatch) -> FenceBatch {
    // SAFETY: this IRQ-masked native owner retains the validated root and restores HGATP/VSATP before return.
    unsafe {
        core::arch::asm!("fence rw, rw", options(nostack));
    }
    let mut performed = FenceBatch {
        translation: false,
        instruction: batch.instruction,
    };
    if batch.instruction {
        // SAFETY: this IRQ-masked native owner retains the validated root and restores HGATP/VSATP before return.
        unsafe {
            crate::entry::publish_guest_code();
        }
    }
    if !batch.translation {
        return performed;
    }
    let root = ctx.hgatp & !MASK;
    // SAFETY: same IRQ-masked owner; an uninitialized cache has no surviving bindings.
    let owner = unsafe { local() };
    if !owner.initialized {
        return performed;
    }
    let Some(tag) = owner.cache.find(Key {
        epoch: ctx.tlb_epoch,
        root,
    }) else {
        return performed;
    };
    let target = root | (usize::from(tag) << SHIFT);
    // SAFETY: this IRQ-masked native owner retains the validated root and restores HGATP/VSATP before return.
    unsafe {
        core::arch::asm!(
            ".option push", ".option arch, +h",
            "csrr {old}, hgatp", "csrr {vsatp}, vsatp", "csrw vsatp, zero",
            "csrw hgatp, {target}", "hfence.vvma zero, zero",
            "csrw hgatp, {old}", "csrw vsatp, {vsatp}", ".option pop",
            old = out(reg) _, vsatp = out(reg) _,
            target = in(reg) target, options(nostack)
        );
    }
    performed.translation = true;
    performed
}

/// Locally retire before dropping a context or handing it to another owner.
/// Preserve actual SIE, including boot/worker code without a native task.
/// The returned bit is acknowledged only after all old implicit walks are
/// terminated; the backend must collect every owner's bit before RAM reuse.
pub(crate) fn retire(ctx: &mut VCpuContext, resident: usize) -> usize {
    let status: usize;
    // SAFETY: this IRQ-masked native owner retains the validated root and restores HGATP/VSATP before return.
    unsafe {
        core::arch::asm!("csrrci {}, sstatus, 2", out(reg) status, options(nostack));
    }
    let bit = 1usize << crate::host::cpu_id();
    let acknowledged = if resident & bit == 0 {
        0
    } else {
        let root = ctx.hgatp & !MASK;
        let key = Key {
            epoch: ctx.tlb_epoch,
            root,
        };
        // SAFETY: a resident bit is recorded only after prepare and a real entry on this CPU.
        let owner = unsafe { local() };
        if let Some(tag) = owner.cache.find(key) {
            let target = root | (usize::from(tag) << SHIFT);
            // SAFETY: this IRQ-masked native owner retains the validated root and restores HGATP/VSATP before return.
            unsafe {
                core::arch::asm!(
                    ".option push", ".option arch, +h",
                    "csrr {old}, hgatp", "csrr {vsatp}, vsatp", "csrw vsatp, zero",
                    "csrw hgatp, {target}", "fence rw, rw",
                    // Non-x0 rs2 also targets VMID zero on no-VMID hardware.
                    "hfence.gvma zero, {tag}", "hfence.vvma zero, zero",
                    "csrw hgatp, {old}", "csrw vsatp, {vsatp}", ".option pop",
                    old = out(reg) _, vsatp = out(reg) _,
                    target = in(reg) target, tag = in(reg) usize::from(tag), options(nostack)
                );
            }
            owner.cache.forget(key);
        }
        ctx.hgatp = root;
        ctx.tlb_flags = 0;
        bit
    };
    if status & 2 != 0 {
        // SAFETY: this IRQ-masked native owner retains the validated root and restores HGATP/VSATP before return.
        unsafe {
            core::arch::asm!("csrsi sstatus, 2", options(nostack));
        }
    }
    acknowledged
}
