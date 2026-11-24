//! Exclusive by-value execution; short metadata locks never span a world switch.
//! Ownership/Lock Ordering: admission bit -> context take -> IRQ bank, then no locks.
//! Invariants: cleanup restores both banks on every Result path before returning CPU.
use super::*;
use crate::vm::devices::DeviceIo;
use deadline::Visit;
struct Owned<V: ArchVcpu> {
    cpu: V,
    state: CpuState,
    start: Option<(usize, usize)>,
    pending: Option<Pending<V>>,
    protected_ticks: u64,
}
impl<V: ArchVcpu> Owned<V> {
    /// Publish the context and all scheduling hints together before releasing
    /// lifecycle ownership. Parked compares exist only while the lane is waiting.
    fn put(self, slot: &VcpuSlot<V>) {
        let mut data = slot.data.lock();
        if self.state == CpuState::Waiting {
            slot.parked_timer.store(
                self.cpu.wait_deadline().unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        } else if data.state == CpuState::Waiting {
            slot.parked_timer.store(u64::MAX, Ordering::Relaxed);
        }
        data.context = Some(self.cpu);
        // An HSM start may have been posted while the stopped context was taken.
        if !(self.state == CpuState::Stopped
            && data.state == CpuState::Starting
            && data.start.is_some())
        {
            data.state = self.state;
        }
        data.pending = self.pending;
        data.protected_ticks = if self.state == CpuState::Runnable {
            self.protected_ticks
        } else {
            0
        };
    }
}
impl<A: HvArch, B: Board> VmRuntime<A, B> {
    pub(super) fn process<R: Tracer>(
        &self,
        vm: &Vm<A>,
        slot: &VcpuSlot<A::Vcpu>,
        local: &mut Dispatcher<A::Timer, R>,
        visit: Visit,
    ) -> Result<bool> {
        if !vm.lifecycle.acquire_owner(slot.index) {
            return Ok(false);
        }
        // Check the live parked compare *after* admission. A hint is a slot
        // number, not a permit to run a recycled or externally woken context.
        if let Visit::Due { offer, .. } = visit {
            let at = slot.parked_timer.load(Ordering::Relaxed);
            let now = A::Timer::now();
            if at != offer.target.at || at > now || now >= offer.until {
                vm.lifecycle.release_owner(slot.index);
                return Ok(false);
            }
        }
        let owned = {
            let mut data = slot.data.lock();
            // A quiet WFI lane only needs its saved interrupt state, not a
            // by-value context round trip. Mailbox work and retirement still
            // take the normal owner path; publishers kick after posting work.
            if visit.peer().is_some()
                && data.state == CpuState::Waiting
                && !vm.lifecycle.stopping()
                && !slot.mailbox.lock().pending()
            {
                if let Some(cpu) = &mut data.context {
                    if !Self::wake_waiter(vm, slot, cpu) {
                        drop(data);
                        vm.lifecycle.release_owner(slot.index);
                        return Ok(false);
                    }
                    data.state = CpuState::Runnable;
                    slot.parked_timer.store(u64::MAX, Ordering::Relaxed);
                }
            }
            data.context.take().map(|cpu| Owned {
                cpu,
                state: data.state,
                start: data.start.take(),
                pending: data.pending.take(),
                protected_ticks: data.protected_ticks,
            })
        };
        let Some(mut owned) = owned else {
            vm.lifecycle.release_owner(slot.index);
            return Err(HvError::InvalidState);
        };
        let result = self.owned_step(vm, slot, &mut owned, local, visit);
        if result.is_err() {
            vm.request_stop(true, false);
        }
        if vm.lifecycle.stopping() || owned.state == CpuState::Stopping {
            self.cancel_pending(vm, slot.index, owned.pending.take());
            owned.cpu.retire_local_translations();
            slot.acknowledge_retired(&owned.cpu);
            owned.state = CpuState::Stopped;
        }
        owned.put(slot);
        vm.lifecycle.release_owner(slot.index);
        // A single VM's error is quarantined in its lifecycle, not propagated
        // as a native dispatcher failure that would stop unrelated guests.
        Ok(result.unwrap_or(false))
    }
    fn owned_step<R: Tracer>(
        &self,
        vm: &Vm<A>,
        slot: &VcpuSlot<A::Vcpu>,
        owned: &mut Owned<A::Vcpu>,
        local: &mut Dispatcher<A::Timer, R>,
        visit: Visit,
    ) -> Result<bool> {
        // SAFETY: matching off-CPU context, banks not loaded, IRQ-masked pinned owner.
        let notify = unsafe { slot.mailbox.lock().drain(&mut owned.cpu) }?;
        self.notify(vm, notify);
        if vm.lifecycle.stopping() {
            return Ok(false);
        }
        if let Some((entry, opaque)) = owned.start.take() {
            owned.cpu.retire_local_translations();
            slot.acknowledge_retired(&owned.cpu);
            owned.cpu.reset_secondary(&vm.space, entry, opaque)?;
            A::publish_code();
            owned.state = CpuState::Runnable;
        }
        if let Some(pending) = &owned.pending {
            let Some(reply) = self.poll_pending(vm, slot.index, pending) else {
                return Ok(false);
            };
            // The fence drain or management poll already consumed its registration.
            // Cancellation is for abandoned work, not a second pass after an ACK.
            owned.pending = None;
            owned.cpu.finish_control(reply)?;
            owned.state = CpuState::Runnable;
        }
        if owned.state == CpuState::Waiting && Self::wake_waiter(vm, slot, &mut owned.cpu) {
            owned.state = CpuState::Runnable;
        }
        let Some(peer) = visit.peer().filter(|_| owned.state == CpuState::Runnable) else {
            return Ok(false);
        };
        let grant = deadline::Grant::new(A::Timer::now(), vm.quantum, peer, owned.protected_ticks)?;
        // SAFETY: only this IRQ-masked owner can enter. Parked/mailbox-only work
        // never borrows a bank; even a partially failed load is put before retirement.
        let mut bank = unsafe { <A::Vcpu as ArchVcpu>::Resident::new() };
        let deadline = match visit {
            Visit::Due { offer, .. } => grant.deadline.min(offer.until),
            _ => grant.deadline,
        };
        let result = self.run_slice(vm, slot, owned, &mut bank, local, deadline);
        if let Ok(entered) = result
            && owned.state == CpuState::Runnable
        {
            // Bank teardown is host work after the slice, not service to this lane.
            owned.protected_ticks = grant.remaining(entered, A::Timer::now);
        }
        bank.put();
        bank.put_interrupts(&mut owned.cpu, &mut vm.devices.irqs.lock())?;
        if matches!(result, Ok(true)) {
            // Stamp the served turn while lifecycle ownership still pins this epoch.
            local.timer_turn = grant.after_entry(
                visit,
                owned.state == CpuState::Waiting,
                || owned.cpu.wait_deadline(),
                A::Timer::now,
                || vm.epoch(),
            );
        }
        result
    }
    // A wake check only makes the vCPU runnable. Guest entry still samples
    // the current line under the IRQ-chip lock before loading its bank.
    fn wake_waiter(vm: &Vm<A>, slot: &VcpuSlot<A::Vcpu>, cpu: &mut A::Vcpu) -> bool {
        let external =
            vm.devices
                .irqs
                .lock()
                .owner_pending::<A::Smp>(slot.index, slot.owner, A::Timer::now);
        cpu.set_external_irq(external);
        cpu.wake_pending()
    }
    fn run_slice<R: Tracer>(
        &self,
        vm: &Vm<A>,
        slot: &VcpuSlot<A::Vcpu>,
        owned: &mut Owned<A::Vcpu>,
        bank: &mut <A::Vcpu as ArchVcpu>::Resident,
        local: &mut Dispatcher<A::Timer, R>,
        deadline: u64,
    ) -> Result<bool> {
        let mut entered = false;
        while vm.lifecycle.can_enter() && A::Timer::now() < deadline {
            slot.entered();
            entered = true;
            {
                // Sample the line and consume its file inbox under the same lock.
                // A producer arriving afterwards belongs to a later observation.
                let mut chip = vm.devices.irqs.lock();
                owned.cpu.set_external_irq(chip.owner_pending::<A::Smp>(
                    slot.index,
                    slot.owner,
                    A::Timer::now,
                ));
                bank.load_interrupts(&mut owned.cpu, &mut chip)?;
            }
            local.tracer.on_vm_enter(slot.index);
            let began = if R::MEASURE_ELAPSED {
                A::Timer::now()
            } else {
                0
            };
            let mut io = DeviceIo {
                devices: &vm.devices,
                origin: slot.index,
                link: self.manager.link(),
                now: A::Timer::now,
                trace: &mut local.tracer,
            };
            // SAFETY: exclusive owned CPU, immutable retained space, scoped bank,
            // sole native timer and one absolute deadline; all locks released.
            let exit = unsafe {
                owned.cpu.run(arch::RunOptions {
                    space: &vm.space,
                    deadline,
                    io: &mut io,
                    timer: &mut local.timer,
                    resident: bank,
                })
            }?;
            let elapsed = if R::MEASURE_ELAPSED {
                A::Timer::now().wrapping_sub(began)
            } else {
                0
            };
            local.tracer.on_vm_exit(slot.index, exit.kind, elapsed);
            if let arch::ExitKind::Control(control) = exit.kind {
                bank.put();
                bank.put_interrupts(&mut owned.cpu, &mut vm.devices.irqs.lock())?;
                let (state, wait) = self.control(vm, slot, &mut owned.cpu, control)?;
                owned.state = state;
                owned.pending = wait;
            } else {
                match exit.kind {
                    arch::ExitKind::Resume => {}
                    arch::ExitKind::Wait => owned.state = CpuState::Waiting,
                    kind => {
                        vm.remember_exit(exit);
                        vm.request_stop(
                            kind == arch::ExitKind::Fault,
                            kind == arch::ExitKind::Reset,
                        );
                    }
                }
            }
            if owned.state != CpuState::Runnable
                || exit.native_event
                || exit.deadline_reached
                || vm.devices.wake.load(Ordering::Acquire) != 0
            {
                break;
            }
        }
        // A near compare may have expired during preparation: no phantom RR turn.
        Ok(entered)
    }
    pub(super) fn notify(&self, vm: &Vm<A>, mut mask: usize) {
        // Completion bits name verified in-VM requesters, never native owners.
        while mask != 0 {
            if let Some(cpu) = vm.cpu(mask.trailing_zeros() as usize) {
                let _ = A::Smp::kick(cpu.owner);
            }
            mask &= mask - 1;
        }
    }
}
