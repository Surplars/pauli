//! Authorized management adapter; only the management owner invokes mutations.
//! Ownership/Lock Ordering: no service/queue lock spans lifecycle or device operations.
//! Invariants: stable public VM IDs are slot+1; replies expose no host memory pointers.
use crate::{
    HvError,
    arch::{ArchTimer, HvArch},
    board::Board,
    runtime::VmRuntime,
    vm::{State, Vm},
};
use pauli_control::{Backend, ChildSpec, ConsoleChunk, Error, Info, MailChunk, MailMessage, VmId};
pub struct Target<'a, A: HvArch, B: Board>(pub &'a VmRuntime<A, B>);
impl<A: HvArch, B: Board> Target<'_, A, B> {
    fn vm(&self, id: VmId) -> Result<impl core::ops::Deref<Target = Vm<A>> + '_, Error> {
        let slot = id.0.checked_sub(1).ok_or(Error::NotFound)? as usize;
        self.0.manager.vm(slot).map_err(|_| Error::NotFound)
    }
}
impl<A: HvArch, B: Board> Backend for Target<'_, A, B> {
    fn stats_read(&self, word: pauli_control::StatsWord) -> Result<u64, Error> {
        if !B::COLLECT_HS_STATS {
            return Err(Error::Unsupported);
        }
        crate::trace::PAULI_HS_STATS.public_word(word)
    }
    fn grant_page(&self) -> Result<&'static mut pauli_control::GrantPage, Error> {
        let pool = self.0.board.domain_pool().ok_or(Error::NoSpace)?;
        let page = pool
            .metadata(1, pauli_control::GrantPage::new)
            .map_err(resource_error)?;
        Ok(&mut page[0])
    }
    fn snapshot(&self) -> impl Iterator<Item = Info> + '_ {
        self.0.manager.vms().map(|vm| info(&vm))
    }
    fn info(&self, vm: VmId) -> core::result::Result<Info, Error> {
        let vm = self.vm(vm)?;
        Ok(info(&vm))
    }
    fn start(&mut self, vm: VmId) -> core::result::Result<bool, Error> {
        if self.info(vm)?.active {
            return Ok(false);
        }
        self.0
            .manager
            .start(self.0.board, vm.0 as usize - 1)
            .map_err(resource_error)?;
        Ok(true)
    }
    fn stop(&mut self, vm: VmId) -> core::result::Result<(), Error> {
        let vm = self
            .0
            .manager
            .vm(vm.0.checked_sub(1).ok_or(Error::NotFound)? as usize)
            .map_err(|_| Error::NotFound)?;
        vm.host_stop();
        Ok(())
    }
    fn create(
        &mut self,
        sponsor: Option<VmId>,
        template: VmId,
        spec: ChildSpec,
    ) -> core::result::Result<Info, Error> {
        let slot = template.0.checked_sub(1).ok_or(Error::NotFound)? as usize;
        let child = self
            .0
            .manager
            .provision(self.0.board, sponsor, slot, spec)
            .map_err(resource_error)?;
        self.info(VmId(child as u32 + 1))
    }
    fn destroy(&mut self, vm: VmId, epoch: u32) -> core::result::Result<(), Error> {
        let slot = vm.0.checked_sub(1).ok_or(Error::NotFound)? as usize;
        self.0.manager.destroy(slot, epoch).map_err(resource_error)
    }
    fn console_read(&self, id: VmId, epoch: u32, cursor: u64) -> Result<ConsoleChunk, Error> {
        let vm = self.vm(id)?;
        // Cold restore resets the log and publishes its new epoch under this same lock.
        let io = vm.devices.io.lock();
        if epoch == 0 || vm.epoch() != epoch {
            return Err(Error::StaleEpoch);
        }
        io.console.read_log(cursor)
    }
    fn mail_send(
        &self,
        sender: VmId,
        source_epoch: u32,
        target: VmId,
        target_epoch: u32,
        message: MailMessage,
    ) -> Result<(), Error> {
        self.mail_send_with_hint(sender, source_epoch, target, target_epoch, message, false)
    }
    fn mail_send_with_hint(
        &self,
        sender: VmId,
        source_epoch: u32,
        target: VmId,
        target_epoch: u32,
        message: MailMessage,
        hint: bool,
    ) -> Result<(), Error> {
        let sender_vm = self.vm(sender)?;
        let target_vm = self.vm(target)?;
        let mut io = target_vm.devices.io.lock();
        if sender_vm.epoch() != source_epoch || target_vm.epoch() != target_epoch {
            return Err(Error::StaleEpoch);
        }
        if !target_vm.lifecycle.can_enter() {
            return Err(Error::NotActive);
        }
        let events = target_vm.config.control_events;
        let result = io.mailbox.send(
            sender,
            source_epoch,
            message,
            events && hint,
            |id, epoch| {
                // Only pins and epoch loads, no other VM's I/O lock. The manager is
                // the sole writer of the directory and cold epochs for this RPC.
                self.vm(id).is_ok_and(|vm| vm.epoch() == epoch)
            },
        );
        // Full-inbox cleanup may have discarded a stale marker even on Busy.
        if events {
            target_vm.devices.refresh_control_event(&mut io);
            drop(io);
            target_vm.deliver_wakes();
        }
        result
    }
    fn mail_recv(
        &self,
        receiver: VmId,
        receiver_epoch: u32,
        peer: VmId,
        peer_epoch: u32,
    ) -> Result<MailChunk, Error> {
        let receiver = self.vm(receiver)?;
        let peer_vm = self.vm(peer)?;
        let mut io = receiver.devices.io.lock();
        if receiver.epoch() != receiver_epoch || peer_vm.epoch() != peer_epoch {
            return Err(Error::StaleEpoch);
        }
        let result = io.mailbox.recv(peer, peer_epoch);
        if receiver.config.control_events && result.message.is_some() {
            receiver.devices.refresh_control_event(&mut io);
            drop(io);
            receiver.deliver_wakes();
        }
        Ok(result)
    }
    fn mail_binding_lost(&self, receiver: VmId, epoch: u32, peer: VmId, peer_epoch: u32) {
        let Ok(vm) = self.vm(receiver) else { return };
        if !vm.config.control_events {
            return;
        }
        let mut io = vm.devices.io.lock();
        if vm.epoch() != epoch {
            return;
        }
        io.mailbox.retract_hints(peer, peer_epoch);
        if vm.lifecycle.can_enter() {
            vm.devices.invalidate_control_event(&mut io);
        } else {
            vm.devices.refresh_control_event(&mut io);
        }
        drop(io);
        vm.deliver_wakes();
    }
    fn input(&mut self, vm: VmId, byte: u8) -> core::result::Result<(), Error> {
        let vm = self
            .0
            .manager
            .vm(vm.0.checked_sub(1).ok_or(Error::NotFound)? as usize)
            .map_err(|_| Error::NotFound)?;
        let accepted = vm.devices.receive_console(byte, A::Timer::now);
        // The line is already visible. Wake its owner(s) after dropping the
        // device locks, including when a full FIFO raised an overrun interrupt.
        vm.deliver_wakes();
        if accepted {
            Ok(())
        } else {
            Err(Error::Backend("guest RX FIFO full"))
        }
    }
}
fn info<A: HvArch>(vm: &Vm<A>) -> Info {
    let state = vm.lifecycle.state();
    Info {
        vm: VmId(vm.slot as u32 + 1),
        epoch: vm.epoch(),
        state: match state {
            State::Ready => pauli_control::State::Running,
            State::Stopping => pauli_control::State::Stopping,
            State::Stopped => pauli_control::State::Stopped,
            State::Faulted => pauli_control::State::Faulted,
        },
        active: state == State::Ready,
        vcpus: vm.config.vcpus,
        memory_bytes: vm.config.memory_size,
    }
}
fn resource_error(error: HvError) -> Error {
    match error {
        HvError::Busy => Error::Busy,
        HvError::Exhausted => Error::NoSpace,
        HvError::NotFound => Error::NotFound,
        HvError::Permission => Error::Denied,
        HvError::Unsupported => Error::Unsupported,
        HvError::InvalidConfig | HvError::InvalidImage => Error::Invalid,
        _ => Error::Backend("VM resource transaction failed"),
    }
}
