//! On-target entry/resume, physical preemption and G-stage permission checks.
//! Ownership/Lock Ordering: one boot owner, private scratch and masked IRQs.
//! Invariants: retire each vCPU before dropping its context; no device is accessed.
use crate::{
    Error, GStagePageTable, VCpuContext, VmExit,
    gstage::{GStageRoot, GStageTable},
    host::{irq::RiscvIrq, timer::read_time},
    timer::PhysicalTimer,
    vcpu::{GuestIo, RunOptions, VirtualCpu},
};
use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, Ordering},
};
use pauli_hal::{
    addr::{MemPerms, PAGE_SIZE, PhysAddr},
    sync::LocalIrq,
};

#[repr(C, align(16384))]
struct Scratch {
    root: GStageRoot,
    tables: [GStageTable; 2],
    code: [u8; PAGE_SIZE],
}
struct Storage(UnsafeCell<Scratch>);
// SAFETY: CLAIMED transfers ownership once, before any guest ever runs.
unsafe impl Sync for Storage {}
static STORAGE: Storage = Storage(UnsafeCell::new(Scratch {
    root: GStageRoot::new(),
    tables: [const { GStageTable::new() }; 2],
    code: [0; PAGE_SIZE],
}));
static CLAIMED: AtomicBool = AtomicBool::new(false);
struct NoDevices;
impl GuestIo for NoDevices {
    fn mmio(&mut self, _: crate::mmio::MmioAccess) -> Option<usize> {
        None
    }
}

/// # Safety
/// Discovered H/HS owner, native timer and Zifencei, no other dispatch on this
/// CPU, before workers. The scratch reservation is linked inside HS memory.
pub unsafe fn probe(gpa: usize, timebase: u64) -> Result<(), Error> {
    if !crate::available() || gpa % PAGE_SIZE != 0 || timebase < 1_000 {
        return Err(Error::Unsupported);
    }
    if CLAIMED.swap(true, Ordering::AcqRel) {
        return Err(Error::Busy);
    }
    // SAFETY: successful unique claim, no concurrent accesses from Rust or a guest.
    let storage = unsafe { &mut *STORAGE.0.get() };
    let code = storage.code.as_mut_ptr();
    // SAFETY: dedicated, aligned writable page, not yet mapped or executing.
    unsafe {
        super::install_minimal(code);
    }
    // SAFETY: root and lower page tables are aligned, private and permanently retained.
    let mut space =
        unsafe { GStagePageTable::new_identity(&mut storage.root, &mut storage.tables) };
    space
        .map_page(
            PhysAddr::new(gpa),
            PhysAddr::new(code as usize),
            MemPerms::READ | MemPerms::EXECUTE,
        )
        .map_err(|_| Error::InvalidImage)?;
    let saved = RiscvIrq::save();
    let result = acceptance(&space, gpa, timebase / 50);
    // SAFETY: acceptance put every CPU, disarmed its TIME endpoint and retained backing.
    unsafe {
        RiscvIrq::restore(saved);
    }
    result
}
fn acceptance(space: &GStagePageTable, base: usize, ticks: u64) -> Result<(), Error> {
    let mut timer = PhysicalTimer::new();
    let result = scenarios(&mut timer, space, base, ticks);
    // SAFETY: probe masked native IRQs and owns the sole temporary TIME endpoint.
    unsafe { timer.disarm() }.map_err(|_| Error::TimerUnavailable)?;
    result
}
fn scenarios(
    timer: &mut PhysicalTimer,
    space: &GStagePageTable,
    base: usize,
    ticks: u64,
) -> Result<(), Error> {
    let mut ctx = VCpuContext::new();
    ctx.pc = base;
    ctx.x[17] = crate::sbi::BASE;
    let mut cpu = VirtualCpu::new(u32::MAX - 2, 0, 1, ctx, space)?;
    let first = step(&mut cpu, space, timer, ticks)?;
    let second = step(&mut cpu, space, timer, ticks)?;
    if first.cause != VmExit::VS_ECALL
        || second.cause != VmExit::VS_ECALL
        || second.pc <= first.pc
        || cpu.context().x[11] != 3
    {
        return Err(Error::HostTransition);
    }
    let mut ctx = VCpuContext::new();
    ctx.pc = base + 0x100; // integer spin fixture
    let mut cpu = VirtualCpu::new(u32::MAX - 1, 0, 1, ctx, space)?;
    if step(&mut cpu, space, timer, ticks)?.cause != VmExit::SUPERVISOR_TIMER
        || cpu.context().x[5] == 0
    {
        return Err(Error::TimerUnavailable);
    }
    let mut ctx = VCpuContext::new();
    ctx.pc = base + 0x80; // store into its own RX page must trap
    ctx.x[11] = base;
    let mut cpu = VirtualCpu::new(u32::MAX, 0, 1, ctx, space)?;
    if step(&mut cpu, space, timer, ticks)?.cause != VmExit::GUEST_STORE_PAGE_FAULT {
        return Err(Error::InvalidState);
    }
    if crate::sstc_available() {
        sstc_switches(space, base, timer, ticks)?;
        pauli_hal::hv_info!("HV selftest PASS: Sstc FULL/LOAD/RESUME compare restoration");
    }
    Ok(())
}

fn sstc_switches(
    space: &GStagePageTable,
    base: usize,
    timer: &mut PhysicalTimer,
    ticks: u64,
) -> Result<(), Error> {
    let mut ctx = VCpuContext::new();
    ctx.enable_sstc()?;
    // SAFETY: enable_sstc succeeded on this IRQ-masked owner. The full host
    // comparator must survive even if host henvcfg.STCE initially was clear.
    let host_compare = || unsafe {
        let value: u64;
        core::arch::asm!("csrr {}, {csr}", out(reg) value, csr = const crate::bits::CSR_VSTIMECMP, options(nostack));
        value
    };
    let host = host_compare();
    let values = [host ^ 1, host ^ 2, host ^ 3];
    ctx.pc = base + 0x1d0;
    ctx.x[12..15].copy_from_slice(&values.map(|value| value as usize));
    ctx.x[17] = crate::sbi::BASE;
    let mut cpu = VirtualCpu::new(u32::MAX - 3, 0, 1, ctx, space)?;
    let mut bank = crate::resident::Resident::default();
    let result = (|| {
        let mut previous = u64::MAX;
        for (i, reg) in [8, 9, 18].into_iter().enumerate() {
            // SAFETY: probe owns this pinned hart, compare and immutable RX page.
            // Execute FULL, resident LOAD and resident RESUME in that order.
            let exit = unsafe {
                cpu.run(RunOptions {
                    space,
                    host_deadline: read_time() + ticks,
                    io: &mut NoDevices,
                    physical: timer,
                    resident: (i != 0).then_some(&mut bank),
                })
            }?;
            if exit.raw.cause != VmExit::VS_ECALL
                || cpu.context().x[reg] as u64 != previous
                || cpu.context().vstimecmp != values[i]
                || host_compare() != host
            {
                return Err(Error::HostTransition);
            }
            previous = values[i];
        }
        Ok(())
    })();
    bank.put(cpu.context_mut());
    cpu.retire_local_translations();
    result
}

fn step(
    cpu: &mut VirtualCpu,
    space: &GStagePageTable,
    physical: &mut PhysicalTimer,
    ticks: u64,
) -> Result<VmExit, Error> {
    // SAFETY: probe owns this masked native CPU and bounded timer, retaining the RX page/tables.
    let result = unsafe {
        cpu.run(RunOptions {
            space,
            host_deadline: read_time() + ticks,
            io: &mut NoDevices,
            physical,
            resident: None,
        })
    };
    cpu.retire_local_translations();
    Ok(result?.raw)
}
