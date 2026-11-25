//! Local command effects and epoch-bound console/grant retirement.
//! Ownership/Lock Ordering: management owner; output occurs outside backend locks.
//! Invariants: stop is asynchronous, detach is not stop, shutdown waits for joined I/O.
use super::*;
impl Monitor {
    pub(super) fn execute<A: HvArch, B: Board>(
        &mut self,
        runtime: &VmRuntime<A, B>,
        command: Command<'_>,
    ) -> Result<()> {
        match command {
            Command::Empty => {}
            Command::Help => self.terminal.output.host(format_args!("vm list/status [name]; vm capacity; vm disks\nvm quota <manager> [<children> <vcpus> <MiB>|clear]\nvm create <template> <name> [--vcpus N] [--memory-mib N] [--disk N]; vm destroy <name>\nvm start <name> [--attach]\nvm attach/stop <name>; vm detach\nvm grant <source> <target> <inspect,start,stop,console,log,mail,create,shape,diskN,destroy|all>\nvm revoke <source> <target>; shutdown\nCtrl+] detaches without stopping the VM\n")),
            Command::Disks => self.disks(runtime),
            Command::Capacity => {
                self.terminal.output.host(format_args!(
                    "VM resources: allocated={} id-limit={} vcpus/VM=1..{}\n",
                    runtime.manager.vms().count(), runtime.manager.capacity(), crate::arch::MAX_VCPUS
                ));
                #[cfg(feature = "selftest")]
                if let Some(([timer, software, external, multiple, none], [lt1, lt5, lt10, ge10])) =
                    runtime.idle_wakes()
                {
                    let ticks = <A::Timer as crate::arch::ArchTimer>::now();
                    self.terminal.output.host(format_args!(
                        "idle-wake timer={timer} software={software} external={external} multiple={multiple} none={none} ticks={ticks} guest_lt1ms={lt1} guest_1to5ms={lt5} guest_5to10ms={lt10} guest_ge10ms={ge10}\n"
                    ));
                }
                #[cfg(feature = "selftest")]
                for owner in 0..runtime.board.info().host_cpus {
                    if let Some((parked, software)) = runtime.worker_wait(owner) {
                        self.terminal.output.host(format_args!(
                            "worker-idle owner={owner} parked={parked} software={software}\n"
                        ));
                    }
                }
            }
            Command::List | Command::Status(None) => self.status(runtime, None)?,
            Command::Quota(name) => self.quota(runtime, name)?,
            Command::SetQuota(name, limit) => {
                runtime.manager.set_budget(runtime.manager.find(name)?, limit)?;
                self.quota(runtime, name)?;
            }
            Command::Status(name) => self.status(runtime, name)?,
            #[cfg(feature = "selftest")]
            Command::Fences(name) => self.fences(runtime, name)?,
            Command::Create(_, _) | Command::Start(_, _) if self.shutdown => return Err(HvError::Busy),
            Command::Create(template, spec) => {
                let slot = self.operation(runtime, template, Operation::Create(spec))?;
                self.terminal.output.host(format_args!("VM {} created: {} (stopped)\n", slot + 1, spec.name.as_str()));
            }
            Command::Destroy(name) => {
                let slot = self.operation(runtime, name, Operation::Destroy)?;
                self.terminal.output.host(format_args!("VM {} destroyed\n", slot + 1));
            }
            Command::Start(name, attach) => {
                let slot = self.operation(runtime, name, Operation::Start)?;
                self.terminal.output.host(format_args!("VM {} started\n", slot + 1));
                if attach { self.attach(runtime, name)?; }
            }
            Command::Attach(name) => self.attach(runtime, name)?,
            Command::Detach => self.terminal.detach(),
            Command::Stop(name) => {
                let slot = self.operation(runtime, name, Operation::Stop)?;
                self.terminal.output.host(format_args!("VM {} stop requested\n", slot + 1));
            }
            Command::Shutdown => {
                self.close(runtime);
            }
            Command::Grant(source, target, rights) => {
                let source = runtime.manager.vm(runtime.manager.find(source)?)?.epoch();
                let target = VmId(runtime.manager.find(target)? as u32 + 1);
                let rights = pauli_control::Rights::parse(rights).ok_or(HvError::InvalidCommand)?;
                self.service.grant(Caller::Host, source, target, rights, &Target(runtime)).map_err(|_| HvError::Permission)?;
            }
            Command::Revoke(source, target) => {
                let source = runtime.manager.vm(runtime.manager.find(source)?)?.epoch();
                let target = VmId(runtime.manager.find(target)? as u32 + 1);
                self.service.revoke(Caller::Host, source, target).map_err(|_| HvError::Permission)?;
            }
        }
        Ok(())
    }
    fn attach<A: HvArch, B: Board>(&mut self, runtime: &VmRuntime<A, B>, name: &str) -> Result<()> {
        let slot = self.operation(runtime, name, Operation::Console)?;
        self.terminal
            .attach(slot, runtime.manager.vm(slot)?.epoch());
        Ok(())
    }
    pub(super) fn notifications<A: HvArch, B: Board>(&mut self, runtime: &VmRuntime<A, B>) -> bool {
        let mut ready = false;
        for vm in runtime.manager.vms() {
            let epoch = vm.epoch();
            let state = vm.lifecycle.state();
            // A failed stop IPI or device join can keep the VM in Stopping.
            // Report it even when there will be no terminal state transition.
            if let Some((faults, join)) = vm.take_fault_notice() {
                self.report_faults(vm.slot + 1, faults, join);
            }
            ready |= state == State::Ready;
            let (old_epoch, previous) = vm.notice.record(epoch, state);
            if old_epoch != epoch {
                if old_epoch != 0 {
                    self.service.retire(old_epoch);
                }
                self.terminal.output.begin(epoch);
            }
            #[cfg(feature = "guest-control")]
            if state == State::Ready && (old_epoch != epoch || previous != Some(State::Ready)) {
                self.boot_grants(runtime, vm.slot);
            }
            if previous != Some(state) && state != State::Ready {
                if previous == Some(State::Ready) {
                    self.service.retire(epoch);
                }
                if !self.terminal.output.direct() {
                    if previous.is_some() {
                        self.terminal.output.host(format_args!(
                            "VM {} epoch={} {:?}\n",
                            vm.slot + 1,
                            epoch,
                            state
                        ));
                    }
                } else if state == State::Faulted {
                    self.terminal.output.host(format_args!(
                        "\n[HV] VM {} epoch={} faulted",
                        vm.slot + 1,
                        epoch
                    ));
                    if let Some(exit) = vm.terminal_exit() {
                        self.terminal.output.host(format_args!(
                            ": cause={:#x} pc={:#x} value={:#x}",
                            exit.cause, exit.pc, exit.value
                        ));
                    }
                    self.terminal.output.host(format_args!("\n"));
                } else if vm.slot == 0 && state == State::Stopped {
                    self.terminal
                        .output
                        .host(format_args!("\n[HV] Linux stopped\n"));
                }
            }
        }
        if self.terminal.attached.is_some_and(|(slot, epoch)| {
            !runtime
                .manager
                .vm(slot)
                .is_ok_and(|vm| vm.epoch() == epoch && !vm.lifecycle.stopping())
        }) {
            self.terminal.detach();
        }
        ready
    }

    #[cfg(feature = "guest-control")]
    fn boot_grants<A: HvArch, B: Board>(&mut self, runtime: &VmRuntime<A, B>, ready: usize) {
        use pauli_control::Rights;

        let boot = runtime.manager.config.count();
        if ready >= boot {
            return;
        }
        let mut grant = |source: &crate::vm::Vm<A>, target: usize, rights| {
            if let Err(error) = self.service.grant(
                Caller::Host,
                source.epoch(),
                VmId(target as u32 + 1),
                rights,
                &Target(runtime),
            ) {
                pauli_hal::hv_error!(
                    "boot service grant VM {}: {}",
                    source.slot + 1,
                    error.message()
                );
            }
        };
        // Only boot-VM Ready edges bind a pair. Repeated polls preserve local
        // revocation, and a service restart cannot refresh its manager's LOG grant.
        for manager_slot in 0..boot {
            let Some(service_slot) = runtime.board.boot_service_for(manager_slot) else {
                continue;
            };
            if ready != manager_slot && ready != service_slot {
                continue;
            }
            let Ok(manager) = runtime.manager.vm(manager_slot) else {
                continue;
            };
            if !manager.lifecycle.can_enter() {
                continue;
            }
            if ready == manager_slot {
                let rights = Rights::INSPECT
                    .union(Rights::START)
                    .union(Rights::STOP)
                    .union(Rights::LOG)
                    .union(Rights::MAIL);
                grant(&manager, service_slot, rights);
            }
            if let Ok(service) = runtime.manager.vm(service_slot)
                && service.lifecycle.can_enter()
            {
                grant(&service, manager_slot, Rights::MAIL);
            }
        }
    }
}
