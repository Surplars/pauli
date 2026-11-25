//! Explicit ISA fixture: CPU state, mailbox, local IRQ mask and monotonic counter.
//! Ownership/Lock Ordering: test CPU is taken by production dispatcher; no native CSRs.
//! Invariants: model fences retire only on the configured owner, never on a sender.
use super::model::{Irqs, NOW, OWNER, Space, Timer};
use crate::{HvError, Result, arch::*};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use pauli_hal::{device::MmioAccess, sync::LocalIrq};
use std::{cell::Cell, collections::VecDeque};

std::thread_local! { static IRQS_MASKED: Cell<bool> = const { Cell::new(false) }; }
pub struct Mask;
impl Mask {
    pub(super) fn active() -> bool {
        IRQS_MASKED.with(Cell::get)
    }
}
// SAFETY: each test thread owns its local mask; nested saves restore the prior value.
unsafe impl LocalIrq for Mask {
    type Saved = bool;
    fn save() -> bool {
        IRQS_MASKED.with(|mask| mask.replace(true))
    }
    unsafe fn restore(saved: bool) {
        IRQS_MASKED.with(|mask| mask.set(saved));
    }
}

pub struct Cpu {
    pub boot_opaque: usize,
    pub epoch: u32,
    pub index: u8,
    pub steps: u64,
    pub retired: bool,
    pub script: VecDeque<ExitKind>,
    pub mmio: Option<MmioAccess>,
    pub reply: Option<ControlReply>,
    pub ipis: u64,
    pub fences: [usize; 2],
    pub external: bool,
    pub deadline: Option<u64>,
    /// Return to the native dispatcher on the next entry, even before its deadline.
    pub native_event: bool,
    /// A compute-only entry runs until its programmed HS deadline unless MMIO is scripted.
    pub run_until_host_timer: bool,
    /// Scripted guest writes its next compare immediately before each WFI.
    pub wfi_timer_interval: Option<u64>,
    /// Fixed-phase guest: advance the last compare, not the time it was serviced.
    pub wfi_timer_period: u64,
}
pub struct Bank {
    base: bool,
    file: bool,
}
impl Resident<Cpu> for Bank {
    unsafe fn new() -> Self {
        Self {
            base: false,
            file: false,
        }
    }
    fn put(&mut self, _cpu: &mut Cpu) {
        self.base = false;
    }
    fn load_interrupts(&mut self, cpu: &mut Cpu, chip: &mut Irqs) -> Result<()> {
        // The line and the inbox consumed for this entry must be one snapshot.
        if cpu.external != chip.lines[cpu.index as usize] {
            chip.split_snapshots += 1;
            return Err(HvError::InvalidState);
        }
        chip.loads += 1;
        self.file = true;
        if core::mem::take(&mut chip.fail_load) {
            Err(HvError::Fault)
        } else {
            Ok(())
        }
    }
    fn put_interrupts(&mut self, _cpu: &mut Cpu, chip: &mut Irqs) -> Result<()> {
        assert!(!self.base, "put the base bank before the IRQ file");
        self.file = false;
        chip.puts += 1;
        Ok(())
    }
}
impl Drop for Bank {
    fn drop(&mut self) {
        assert!(!self.base && !self.file, "resident state escaped its owner");
    }
}
impl ArchVcpu for Cpu {
    type Space = Space;
    type Irq = Irqs;
    type Timer = Timer;
    type Resident = Bank;
    type Mailbox = Inbox;
    fn create(
        _space: &Space,
        index: u8,
        epoch: u32,
        _config: &pauli_config::VmConfig,
        boot: VcpuBoot,
    ) -> Result<Self> {
        Ok(Self {
            boot_opaque: boot.opaque,
            epoch,
            index,
            steps: 0,
            retired: true,
            script: VecDeque::new(),
            mmio: None,
            reply: None,
            ipis: 0,
            fences: [0; 2],
            external: false,
            deadline: None,
            native_event: false,
            run_until_host_timer: false,
            wfi_timer_interval: None,
            wfi_timer_period: 0,
        })
    }
    fn reset_secondary(&mut self, _space: &Space, _entry: usize, _opaque: usize) -> Result<()> {
        if !self.retired {
            return Err(HvError::Busy);
        }
        Ok(())
    }
    unsafe fn run(&mut self, opts: RunOptions<'_, Self>) -> Result<VcpuExit> {
        unsafe { self.run_with_trace(opts, None) }
    }
    unsafe fn run_with_trace(
        &mut self,
        opts: RunOptions<'_, Self>,
        mut sample: Option<&mut GuestSample>,
    ) -> Result<VcpuExit> {
        assert!(opts.resident.file);
        opts.resident.base = true;
        self.retired = false;
        self.steps += 1;
        TickInterval::measure(
            sample.as_deref_mut().map(|s| &mut s.interval),
            Timer::now,
            || {
                NOW.with(|now| {
                    now.set(if self.run_until_host_timer && self.mmio.is_none() {
                        opts.deadline
                    } else {
                        now.get() + 100
                    })
                });
            },
        );
        let mmio = self.mmio.take();
        if let Some(access) = mmio {
            assert!(opts.io.mmio(access).is_some());
        }
        let kind = self.script.pop_front().unwrap_or(ExitKind::Resume);
        if kind == ExitKind::Wait {
            if self.wfi_timer_period != 0 {
                self.deadline = Some(self.deadline.unwrap_or(0) + self.wfi_timer_period);
            } else if let Some(interval) = self.wfi_timer_interval {
                self.deadline = Some(Timer::now() + interval);
            }
        }
        if let Some(sample) = sample {
            sample.source = if kind == ExitKind::Wait {
                ExitSource::Wfi
            } else if mmio.is_some() {
                ExitSource::Mmio
            } else {
                ExitSource::Other
            };
        }
        Ok(VcpuExit {
            kind,
            cause: 0,
            pc: 0,
            value: 0,
            deadline_reached: Timer::now() >= opts.deadline,
            native_event: core::mem::take(&mut self.native_event),
        })
    }
    fn set_external_irq(&mut self, asserted: bool) {
        self.external = asserted;
    }
    fn inject_ipi(&mut self) {
        self.ipis += 1;
    }
    fn wait_deadline(&self) -> Option<u64> {
        self.deadline
    }
    fn wake_pending(&mut self) -> bool {
        self.external || self.ipis != 0 || self.deadline.is_some_and(|d| d <= Timer::now())
    }
    fn finish_control(&mut self, reply: ControlReply) -> Result<()> {
        self.reply = Some(reply);
        Ok(())
    }
    unsafe fn synchronize(&mut self, instruction: bool) {
        assert!(
            Mask::active(),
            "local synchronization requires masked host IRQs"
        );
        self.fences[usize::from(instruction)] += 1;
    }
    fn retire_local_translations(&mut self) {
        self.retired = true;
    }
    fn translations_retired(&self) -> bool {
        self.retired
    }
}
#[derive(Clone, Copy)]
pub struct Ticket {
    kind: usize,
    generation: u64,
}
pub struct Inbox {
    epoch: u32,
    index: u8,
    owner: usize,
    requests: [AtomicU64; 2],
    done: [AtomicU64; 2],
    waiters: AtomicUsize,
    pub(super) disarms: Cell<usize>,
    pub(super) fail_drain: Cell<bool>,
    ipi: AtomicBool,
    // One seeded arch observation for exercising the real monitor/epoch boundary.
    #[cfg(feature = "selftest")]
    pub(super) observed: Option<(bool, usize, u8, u64)>,
    #[cfg(feature = "selftest")]
    pub(super) ready: Cell<Option<(bool, usize, u8, bool)>>,
    #[cfg(feature = "selftest")]
    pub(super) ready_count: Cell<usize>,
}
impl Inbox {
    pub(super) fn armed(&self) -> usize {
        self.waiters.load(Ordering::Acquire)
    }
}
impl Mailbox<Cpu> for Inbox {
    type Ticket = Ticket;
    fn new(epoch: u32, index: u8, owner: usize) -> Self {
        Self {
            epoch,
            index,
            owner,
            requests: [const { AtomicU64::new(0) }; 2],
            done: [const { AtomicU64::new(0) }; 2],
            waiters: AtomicUsize::new(0),
            disarms: Cell::new(0),
            fail_drain: Cell::new(false),
            ipi: AtomicBool::new(false),
            #[cfg(feature = "selftest")]
            observed: None,
            #[cfg(feature = "selftest")]
            ready: Cell::new(None),
            #[cfg(feature = "selftest")]
            ready_count: Cell::new(0),
        }
    }
    fn post_ipi(&self) {
        self.ipi.store(true, Ordering::Release);
    }
    fn post_fence(&self, instruction: bool, requester: u8) -> Ticket {
        let kind = instruction as usize;
        let generation = self.requests[kind].fetch_add(1, Ordering::Release) + 1;
        self.waiters.fetch_or(1 << requester, Ordering::Release);
        Ticket { kind, generation }
    }
    fn completed(&self, ticket: Ticket) -> bool {
        self.done[ticket.kind].load(Ordering::Acquire) >= ticket.generation
    }
    #[cfg(feature = "selftest")]
    fn fence_sample(&self, instruction: bool, generation: usize) -> Option<FenceSample> {
        let (kind, seq, requester, ticks) = self.observed?;
        if (kind, seq) != (instruction, generation) {
            return None;
        }
        let deferred = self.ready.get().and_then(|(kind, seq, cpu, deferred)| {
            ((kind, seq, cpu) == (instruction, generation, requester)).then_some(deferred)
        });
        Some(FenceSample {
            requester,
            ticks,
            deferred,
        })
    }
    #[cfg(feature = "selftest")]
    fn note_fence_ready(&self, ticket: Ticket, requester: u8, deferred: bool) {
        assert!(
            self.completed(ticket),
            "cannot classify before the target ACK"
        );
        self.ready.set(Some((
            ticket.kind != 0,
            ticket.generation as usize,
            requester,
            deferred,
        )));
        self.ready_count.set(self.ready_count.get() + 1);
    }
    fn disarm(&self, requester: u8) {
        self.disarms.set(self.disarms.get() + 1);
        self.waiters.fetch_and(!(1 << requester), Ordering::AcqRel);
    }
    fn pending(&self) -> bool {
        self.ipi.load(Ordering::Acquire)
            || self.waiters.load(Ordering::Acquire) != 0
            || (0..2).any(|kind| {
                self.requests[kind].load(Ordering::Acquire)
                    != self.done[kind].load(Ordering::Acquire)
            })
    }
    unsafe fn drain(&self, cpu: &mut Cpu) -> Result<usize> {
        assert_eq!(
            (cpu.epoch, cpu.index, OWNER.with(|o| o.get())),
            (self.epoch, self.index, self.owner)
        );
        if self.fail_drain.replace(false) {
            return Err(HvError::Fault);
        }
        let wake = self.waiters.swap(0, Ordering::AcqRel);
        for kind in 0..2 {
            let generation = self.requests[kind].load(Ordering::Acquire);
            if self.done[kind].load(Ordering::Relaxed) == generation {
                continue;
            }
            // SAFETY: fixture checks the production dispatcher's pinned-owner selection above.
            unsafe {
                cpu.synchronize(kind != 0);
            }
            self.done[kind].store(generation, Ordering::Release);
        }
        if self.ipi.swap(false, Ordering::AcqRel) {
            cpu.inject_ipi();
        }
        Ok(wake)
    }
}
