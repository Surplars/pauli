//! VM-local CPU-control requests; native harts are never guest-addressable.
//! Ownership/Lock Ordering: current CPU is owned by value; lock one target at a time.
//! Invariants: validate the entire mask before side effects; fence reply follows ACKs.
use super::*;
use crate::arch::Stage2Space;
use arch::{Control, ControlError as Error, ControlReply as Reply};
impl<A: HvArch, B: Board> VmRuntime<A, B> {
    pub(super) fn control(
        &self,
        vm: &Vm<A>,
        current: &VcpuSlot<A::Vcpu>,
        cpu: &mut A::Vcpu,
        action: Control,
    ) -> Result<(CpuState, Option<Pending<A::Vcpu>>)> {
        let reply = match action {
            Control::Start {
                cpu: target,
                entry,
                opaque,
            } => self.start_cpu(vm, target, entry, opaque),
            Control::StopCpu => {
                current.data.lock().state = CpuState::Stopping;
                return Ok((CpuState::Stopping, None));
            }
            Control::CpuStatus { cpu: target } => match vm.cpu(target) {
                Some(slot) => Reply::CpuStatus(slot.status()),
                None => Reply::Error(Error::InvalidParameter),
            },
            Control::Ipi { mask, base } => self.ipi(vm, mask, base),
            Control::Fence {
                mask,
                base,
                instruction,
            } => match arch::cpu_targets(mask, base, usize::from(vm.config.vcpus)) {
                Some(mask) => {
                    let wait = self.fence(vm, current, cpu, mask, instruction)?;
                    if !self.fences_complete(vm, &wait, current.index, false) {
                        return Ok((CpuState::Blocked, Some(Pending::Fence(wait))));
                    }
                    Reply::Success(0)
                }
                None => Reply::Error(Error::InvalidParameter),
            },
            #[cfg(feature = "guest-control")]
            Control::ManagementProbe => Reply::Transport {
                error: 0,
                value: pauli_control::abi::probe(self.manager.capacity() as u32),
            },
            #[cfg(feature = "guest-control")]
            Control::Events(control) => match vm.control_event(control, current.index) {
                Ok(value) => {
                    vm.deliver_wakes();
                    Reply::Transport { error: 0, value }
                }
                Err(error) => Reply::Transport { error, value: 0 },
            },
            #[cfg(feature = "guest-control")]
            Control::Management { args } => {
                match crate::monitor::rpc::submit(self, vm.slot, current.index, vm.epoch(), args) {
                    Ok(ticket) => {
                        return Ok((CpuState::Blocked, Some(Pending::Management(ticket))));
                    }
                    Err(result) => Reply::Transport {
                        error: result.error,
                        value: result.value,
                    },
                }
            }
        };
        cpu.finish_control(reply)?;
        Ok((CpuState::Runnable, None))
    }
    fn start_cpu(&self, vm: &Vm<A>, target: usize, entry: usize, opaque: usize) -> Reply {
        let Some(slot) = vm.cpu(target) else {
            return Reply::Error(Error::InvalidParameter);
        };
        if !vm.space.executable(entry) {
            return Reply::Error(Error::InvalidAddress);
        }
        match slot.request_start(entry, opaque) {
            Ok(()) => match Self::kick_posted(vm, slot) {
                Ok(()) => Reply::Success(0),
                Err(_) => Reply::Error(Error::Failed),
            },
            Err(_) => Reply::Error(Error::AlreadyAvailable),
        }
    }
    fn ipi(&self, vm: &Vm<A>, mask: usize, base: usize) -> Reply {
        let Some(mask) = arch::cpu_targets(mask, base, usize::from(vm.config.vcpus)) else {
            return Reply::Error(Error::InvalidParameter);
        };
        for slot in vm.cpus() {
            if mask & (1 << slot.index) != 0 && slot.status() != arch::CpuStatus::Stopped {
                slot.mailbox.lock().post_ipi();
                if Self::kick_posted(vm, slot).is_err() {
                    return Reply::Error(Error::Failed);
                }
            }
        }
        Reply::Success(0)
    }

    // The HSM/mailbox work is already visible. Keep the original SBI error
    // and let management retry the wake if the target is in untimed WFI.
    fn kick_posted(vm: &Vm<A>, slot: &VcpuSlot<A::Vcpu>) -> Result<()> {
        A::Smp::kick(slot.owner).inspect_err(|_| {
            vm.devices.wake.fetch_or(1 << slot.index, Ordering::Release);
        })
    }
    fn fence(
        &self,
        vm: &Vm<A>,
        current: &VcpuSlot<A::Vcpu>,
        cpu: &mut A::Vcpu,
        mask: usize,
        instruction: bool,
    ) -> Result<FenceWait<A::Vcpu>> {
        let mut wait = FenceWait {
            tickets: [None; arch::MAX_VCPUS],
        };
        for target in vm.cpus() {
            if mask & (1 << target.index) == 0 {
                continue;
            }
            if target.index == current.index {
                // SAFETY: current CPU is off-CPU, banks put and IRQs masked.
                unsafe {
                    cpu.synchronize(instruction);
                }
            } else {
                let posted = if target.owner == current.owner {
                    self.local_fence(vm, target, current.index, instruction)
                } else {
                    let ticket = target.mailbox.lock().post_fence(instruction, current.index);
                    A::Smp::kick(target.owner).map(|()| ticket)
                };
                match posted {
                    Ok(ticket) => wait.tickets[target.index as usize] = Some(ticket),
                    Err(error) => {
                        target.mailbox.lock().disarm(current.index);
                        self.cancel_wait(vm, current.index, &wait);
                        return Err(error);
                    }
                }
            }
        }
        Ok(wait)
    }

    /// The requester pins this VM epoch. Its distinct sibling is off-CPU on
    /// this very hart, so we can fulfill the obligation without scheduling it.
    /// Keep the borrowed sibling context off the dedicated-hart caller's stack.
    #[inline(never)]
    fn local_fence(
        &self,
        vm: &Vm<A>,
        target: &VcpuSlot<A::Vcpu>,
        requester: u8,
        instruction: bool,
    ) -> Result<crate::vm::slot::Ticket<A::Vcpu>> {
        let mut cpu = target
            .data
            .lock()
            .context
            .take()
            .ok_or(HvError::InvalidState)?;
        let (ticket, notify) = {
            let mailbox = target.mailbox.lock();
            let ticket = mailbox.post_fence(instruction, requester);
            // SAFETY: pinned IRQ-masked owner; source banks are put, sibling
            // context is exclusively borrowed, and neither vCPU is executing.
            (ticket, unsafe { mailbox.drain(&mut cpu) })
        };
        // Restore even on drain error. A concurrent HSM start still owns its
        // start/state fields; never replace the entire target slot here.
        target.data.lock().context = Some(cpu);
        self.notify(vm, notify? & !(1usize << requester));
        Ok(ticket)
    }
    pub(super) fn fences_complete(
        &self,
        vm: &Vm<A>,
        wait: &FenceWait<A::Vcpu>,
        _requester: u8,
        _deferred: bool,
    ) -> bool {
        // A partial fan-out ACK does not identify when the whole SBI call was ready.
        #[cfg(feature = "selftest")]
        let single = wait.tickets.iter().flatten().take(2).count() == 1;
        vm.cpus().iter().all(|slot| {
            let Some(ticket) = wait.tickets[slot.index as usize] else {
                return true;
            };
            let mailbox = slot.mailbox.lock();
            let ready = mailbox.completed(ticket);
            #[cfg(feature = "selftest")]
            if single && ready {
                mailbox.note_fence_ready(ticket, _requester, _deferred);
            }
            ready
        })
    }
    pub(super) fn cancel_wait(&self, vm: &Vm<A>, requester: u8, wait: &FenceWait<A::Vcpu>) {
        for slot in vm.cpus() {
            if wait.tickets[slot.index as usize].is_some() {
                slot.mailbox.lock().disarm(requester);
            }
        }
    }
}
