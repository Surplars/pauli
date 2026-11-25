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
        self.kick_stopping_owners();
    }
    pub fn host_stop(&self) {
        self.lifecycle.cancel_reset();
        self.kick_stopping_owners();
    }
    fn kick_stopping_owners(&self) {
        if let Err(fault) = self.kick_owners() {
            self.record_shutdown_kick(fault);
        }
    }
    fn kick_owners(&self) -> core::result::Result<(), KickFault> {
        // Start/stop/retirement concern every owner, independent of IRQ routing.
        let epoch = self.epoch();
        self.kick_lanes(self.devices.lanes, A::Smp::kick_detailed)
            .map_err(|(owner, source)| KickFault {
                epoch,
                owner,
                source,
            })
    }
    fn kick_lanes<E>(
        &self,
        mut lanes: usize,
        kick: impl Fn(usize) -> core::result::Result<(), E>,
    ) -> core::result::Result<(), (usize, E)> {
        let mut seen = 0u64; // A shared physical owner needs only one IPI.
        let mut retry = 0;
        let mut result = Ok(());
        while lanes != 0 {
            let index = lanes.trailing_zeros() as usize;
            lanes &= lanes - 1;
            let owner = self.cpus[index].owner;
            let bit = 1u64 << owner;
            if seen & bit == 0 {
                if let Err(error) = kick(owner) {
                    // One lane represents the failed owner; continue with its peers.
                    retry |= 1usize << index;
                    if result.is_ok() {
                        result = Err((owner, error));
                    }
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
            let _ = self.kick_lanes(self.devices.wake.swap(0, Ordering::AcqRel), A::Smp::kick);
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
    pub fn start<B: Board>(
        &self,
        board: &B,
        epoch: u32,
        link: &SpinLock<Link>,
        now: u64,
    ) -> Result<()> {
        if !self.lifecycle.joined() || !self.retired() || self.join_error().is_some() {
            return Err(HvError::Busy);
        }
        let rebuild = self.lifecycle.begin_rebuild()?;
        let old = self.started.swap(true, Ordering::AcqRel);
        let epoch = if old { epoch } else { self.epoch() };
        self.begin_activation();
        *self.terminal.lock() = None;
        if let Err(error) = self.prepare_start(board, epoch, old, link, now) {
            drop(rebuild);
            self.activation_failed(epoch, error);
            // A partial activation may already have reset a disk or bound a port.
            self.lifecycle.require_device_join();
            return Err(error);
        }
        if let Err(error) = rebuild.publish() {
            // A concurrent stop cancelled admission. Drain prepared devices.
            self.record_activation(ActivationFault::Activation {
                epoch,
                owner: A::Smp::cpu_id(),
                error,
            });
            self.lifecycle.require_device_join();
            return Err(error);
        }
        // Report the original start IPI separately from any failed stop IPI.
        if let Err(fault) = self.kick_owners() {
            self.record_activation(ActivationFault::StartKick(fault));
            self.request_stop(true, false);
            return Err(fault.source.error);
        }
        Ok(())
    }

    pub(crate) fn activation_failed(&self, epoch: u32, error: HvError) {
        self.record_activation(ActivationFault::Activation {
            epoch,
            owner: A::Smp::cpu_id(),
            error,
        });
        self.lifecycle.request_stop(true, false);
    }

    fn prepare_start<B: Board>(
        &self,
        board: &B,
        epoch: u32,
        old: bool,
        link: &SpinLock<Link>,
        now: u64,
    ) -> Result<()> {
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
            // Publish the new epoch under this I/O lock so a log reader cannot see
            // reset history under the old epoch, or previous bytes under the new one.
            io.console.reset((hz / 1000).max(1));
            io.mailbox.reset();
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
        Ok(())
    }
}
