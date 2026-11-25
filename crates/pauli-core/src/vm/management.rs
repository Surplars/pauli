//! Joined VM start/stop/reset transactions, without a parallel VM registry.
//! Ownership/Lock Ordering: management is single-writer; owner admission freezes at join.
//! Invariants: never rewrite RAM or mailbox generations before all CPU/I/O proofs.
use super::*;
use crate::{
    HvError, Result,
    arch::{ArchSmp, ArchVcpu, VirtualIrqChip},
    board::Board,
};
use pauli_devices::net_link::Link;
use pauli_hal::sync::SpinLock;
impl<A: HvArch> Vm<A> {
    pub fn request_stop(&self, fault: bool, reset: bool) {
        self.lifecycle.request_stop(fault, reset);
        let _ = self.kick_owners(); // Stop stays closed even if a native IPI fails.
    }
    pub fn host_stop(&self) {
        self.lifecycle.cancel_reset();
        let _ = self.kick_owners();
    }
    fn kick_owners(&self) -> Result<()> {
        // Start/stop/retirement concern every owner, independent of IRQ routing.
        self.kick_lanes(self.devices.lanes)
    }
    fn kick_lanes(&self, mut lanes: usize) -> Result<()> {
        let mut seen = 0u64; // A shared physical owner needs only one IPI.
        let mut retry = 0;
        let mut result = Ok(());
        while lanes != 0 {
            let index = lanes.trailing_zeros() as usize;
            lanes &= lanes - 1;
            let owner = self.cpus[index].owner;
            let bit = 1u64 << owner;
            if seen & bit == 0 {
                if let Err(error) = A::Smp::kick(owner) {
                    // One lane represents the failed owner; continue with its peers.
                    retry |= 1usize << index;
                    result = Err(error);
                }
                seen |= bit;
            }
        }
        if retry != 0 {
            // Preserve work published while the native IPIs were in flight.
            self.devices.wake.fetch_or(retry, Ordering::Release);
        }
        result
    }
    pub fn deliver_wakes(&self) {
        // Empty service polls must not take the producer's cache line for writing.
        // A publication after this check remains pending for the next pass.
        if self.devices.wake.load(Ordering::Relaxed) != 0 {
            #[cfg(feature = "selftest")]
            if super::irq_bank::RxObservation::enabled(&self.config) {
                let owner = self.cpus[0].owner;
                if owner != A::Smp::cpu_id() {
                    self.deliver_probe_wake(owner);
                    return;
                }
            }
            let _ = self.kick_lanes(self.devices.wake.swap(0, Ordering::AcqRel));
        }
    }
    #[cfg(feature = "selftest")]
    fn deliver_probe_wake(&self, owner: usize) {
        use crate::arch::ArchTimer;
        // Serialise taking this single lane's wake with publishing its edge.
        // Release the chip before SBI: the receiver can run before send returns.
        let ticket = {
            let mut chip = self.devices.irqs.lock();
            if self.devices.wake.swap(0, Ordering::AcqRel) == 0 {
                return;
            }
            chip.rx.take_send(self.epoch(), owner, A::Smp::ack_sequence)
        };
        // Only this single remote lane was drained. Timestamp the native call,
        // not contention on the chip lock; the target may already be back in VS
        // before the sender returns. Failed kicks retain the normal retry bit.
        let attempt = ticket.map(|t| (t, A::Timer::now()));
        if A::Smp::kick(owner).is_err() {
            self.devices.wake.fetch_or(1, Ordering::Release);
        } else if let Some((ticket, sent)) = attempt {
            let returned = A::Timer::now();
            self.devices
                .irqs
                .lock()
                .rx
                .confirm(self.epoch(), ticket, sent, returned);
        }
    }
    pub fn try_join(&self, now: u64, link: &SpinLock<Link>) -> Result<bool> {
        if let Some(error) = self.join_error() {
            return Err(error); // An unproven retirement cannot be retried into a new owner.
        }
        if self.lifecycle.joined() {
            return Ok(true);
        }
        if !self.lifecycle.stopping() || !self.lifecycle.owners_idle() || !self.retired() {
            return Ok(false);
        }
        let devices = self.devices.stop(now, link)?;
        Ok(self.lifecycle.commit_join(self.retired(), devices))
    }
    pub(crate) fn fail_join(&self, error: HvError) {
        if self.join_error.set(error).is_ok() {
            self.request_stop(true, false);
        }
    }
    pub fn start<B: Board>(
        &self,
        board: &B,
        epoch: u32,
        link: &SpinLock<Link>,
        now: u64,
    ) -> Result<()> {
        if !self.lifecycle.joined() || !self.retired() {
            return Err(HvError::Busy);
        }
        let rebuild = self.lifecycle.begin_rebuild()?;
        let old = self.started.swap(true, Ordering::AcqRel);
        if old {
            let hz = board.info().timebase_hz;
            let unix = match board.rtc() {
                Some(rtc) => rtc.unix_ns()?,
                None => 0,
            };
            let chip = A::Irq::create_with_imsic_ids(
                self.config.vcpus,
                self.config.aia,
                self.devices.layout.imsic_ids,
            )?;
            for slot in self.cpus() {
                let cpu = A::Vcpu::create(&self.space, slot.index, epoch, &self.config, self.boot)?;
                slot.replace_joined(cpu, epoch)?;
            }
            let mut io = self.devices.io.lock();
            // SAFETY: Joined freezes all owners; try_join drained producers and
            // proved every visited native CPU retired translations before clear.
            unsafe { io.ram.restore_retired(&self.image) }?;
            A::publish_code();
            io.console.reset((hz / 1000).max(1));
            io.rtc = crate::backends::rtc::Rtc::new(unix, now, hz);
            self.devices.replace_chip(&mut io, chip, &self.config);
            self.epoch.store(epoch, Ordering::Release);
        }
        let mut io = self.devices.io.lock();
        if let Some(block) = &mut io.block {
            block.reset()?;
        }
        if let Some(net) = &mut io.net {
            net.activate(self.epoch(), link)?;
        }
        drop(io);
        *self.terminal.lock() = None;
        if let Err(error) = rebuild.publish() {
            // No CPU ever entered this activation. If a concurrent stop
            // cancelled publication, drain devices before a later retry.
            self.lifecycle.require_device_join();
            return Err(error);
        }
        // The first IPI may be the only wake source of a previously unassigned
        // worker. Never report a successful start if firmware rejected it.
        self.kick_owners()
            .inspect_err(|_| self.request_stop(true, false))
    }
}
