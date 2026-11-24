//! Cross-hart requests for one pinned vCPU epoch. Only its physical owner may
//! touch the context; senders publish atomics before issuing an SBI host kick.
//! Fence tickets are acknowledged AFTER the target executes its local fence.
//! Ownership/Lock Ordering: senders publish atomics; only the pinned IRQ-masked CPU mutates context.
//! Invariants: clear a physical kick once before scanning; fence ACK follows actual completion.
use crate::Error as HvError;
use crate::host;
use crate::vcpu::VirtualCpu;
use core::sync::atomic::{AtomicBool, Ordering};

use super::FenceTicket;
use super::fence::FenceQueue;

/// Clear before observing mailbox publications, including before an idle WFI.
/// # Safety
/// HS owner with IRQs masked; this hart's physical SSIP belongs to this queue.
pub unsafe fn acknowledge_kick() {
    host::irq::clear_software_interrupt(
        #[cfg(feature = "selftest")]
        pauli_core::arch::NativeAckPath::DispatchAck,
    );
    // SAFETY: the HS owner orders the physical ACK before reading the directory
    // or mailboxes; a publication after the scan must leave a new kick pending.
    unsafe { core::arch::asm!("fence o, r", options(nostack)) };
}

pub struct VcpuMailbox {
    id: u32,
    index: u8,
    hart: usize,
    software: AtomicBool,
    fences: FenceQueue,
    #[cfg(feature = "selftest")]
    samples: super::samples::Samples,
}
impl VcpuMailbox {
    /// The dispatcher pins this vCPU to `hart` until all senders have joined.
    /// Allocate a NEW mailbox for a new VM epoch; never reuse outstanding work.
    pub const fn new(id: u32, index: u8, hart: usize) -> Self {
        Self {
            id,
            index,
            hart,
            software: AtomicBool::new(false),
            fences: FenceQueue::new(),
            #[cfg(feature = "selftest")]
            samples: super::samples::Samples::new(),
        }
    }
    pub(crate) fn post_ipi(&self) {
        self.software.store(true, Ordering::Release);
    }
    /// Register a completion wake for a validated virtual requester, not a
    /// physical hart supplied by the guest. Drain returns its bit AFTER fencing.
    pub(crate) fn post_fence(&self, instruction: bool, requester: u8) -> FenceTicket {
        let ticket = self.fences.request(instruction);
        self.fences.arm(requester);
        ticket
    }
    pub fn disarm_fence_notify(&self, requester: u8) {
        self.fences.disarm(requester);
    }
    pub fn completed(&self, ticket: FenceTicket) -> bool {
        self.fences.completed(ticket)
    }
    #[cfg(feature = "selftest")]
    pub(crate) fn fence_sample(
        &self,
        instruction: bool,
        generation: usize,
    ) -> Option<pauli_core::arch::FenceSample> {
        let (requester, ticks) = self.samples.get(instruction, generation)?;
        Some(pauli_core::arch::FenceSample {
            requester,
            ticks,
            deferred: self.samples.deferred(instruction, generation),
        })
    }
    #[cfg(feature = "selftest")]
    pub(crate) fn note_ready(&self, ticket: FenceTicket, requester: u8, deferred: bool) {
        self.samples.note_ready(ticket, requester, deferred);
    }
    /// A false observation is not a consumed wake: every subsequent remote
    /// publication also kicks this physical owner, including across VS entry.
    pub fn pending(&self) -> bool {
        self.software.load(Ordering::Acquire) || self.fences.pending()
    }
    /// Service requests before selection, including off-CPU parked requesters.
    /// Return in-VM completion bits to kick AFTER local fencing/queue mutation.
    /// # Safety
    /// This physical hart owns the matching off-CPU vCPU, IRQs are masked, and
    /// its tables/backing remain live. Call before every entry and during waits.
    pub unsafe fn drain(&self, cpu: &mut VirtualCpu) -> Result<usize, HvError> {
        debug_assert_eq!(cpu.id(), self.id);
        debug_assert_eq!(cpu.index(), self.index);
        debug_assert_eq!(host::cpu_id() as usize, self.hart);
        // Physical SSIP is shared by every vCPU on this hart. Its owner clears
        // it ONCE before scanning all local mailboxes, not once per vCPU: a
        // later clear could erase an earlier sibling's new wake before WFI.
        let notify = self.fences.drain_with(
            // SAFETY: the pinned IRQ-masked owner synchronizes its matching off-CPU context.
            |batch| unsafe { cpu.synchronize_translations(batch) },
            |_prior, _next, _notify| {
                #[cfg(feature = "selftest")]
                self.samples.record(_prior, _next, _notify, completion_time);
            },
        );
        // As with completion registrations, leave late publications latched
        // for the sender's subsequent kick instead of writing on an empty poll.
        if self.software.load(Ordering::Acquire) && self.software.swap(false, Ordering::AcqRel) {
            cpu.inject_software_interrupt();
        }
        Ok(notify)
    }
}

#[cfg(feature = "selftest")]
fn completion_time() -> u64 {
    let ticks: u64;
    // SAFETY: HS may read TIME. Do not use nomem: the compiler must keep
    // this observation between synchronization and the release of the ACK.
    unsafe { core::arch::asm!("rdtime {}", out(reg) ticks, options(nostack)) };
    ticks
}
