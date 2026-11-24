//! Host-only status formatting, performed after taking small immutable snapshots.
//! Ownership/Lock Ordering: release each CPU/device lock before console output.
//! Invariants: reporting never mutates queues, guest state or lifetime ownership.
use super::*;
impl Monitor {
    /// Joined freezes this incarnation's mailboxes. Copy one observation and
    /// release its guard before formatting; never delay a live fence for tracing.
    #[cfg(feature = "selftest")]
    pub(super) fn fences<A: HvArch, B: Board>(
        &mut self,
        runtime: &VmRuntime<A, B>,
        name: &str,
    ) -> Result<()> {
        use crate::arch::{FENCE_SAMPLE_LIMIT, FenceSample, Mailbox};
        let vm = runtime.manager.vm(runtime.manager.find(name)?)?;
        if !vm.lifecycle.joined() {
            return Err(HvError::Busy);
        }
        let epoch = vm.epoch();
        for cpu in vm.cpus() {
            for (instruction, op) in [(false, "vma"), (true, "fencei")] {
                for generation in 1..=FENCE_SAMPLE_LIMIT {
                    let sample = cpu.mailbox.lock().fence_sample(instruction, generation);
                    if let Some(FenceSample {
                        requester,
                        ticks,
                        deferred,
                    }) = sample
                    {
                        let path = match deferred {
                            Some(false) => "direct",
                            Some(true) => "deferred",
                            None => "unobserved",
                        };
                        self.terminal.output.host(format_args!(
                            "fence epoch={epoch} target={} owner={} requester={requester} op={op} gen={generation} ticks={ticks} path={path}\n",
                            cpu.index, cpu.owner
                        ));
                    }
                }
            }
        }
        Ok(())
    }
    pub(super) fn disks<A: HvArch, B: Board>(&mut self, runtime: &VmRuntime<A, B>) {
        for (n, disk) in runtime.manager.config.disks.iter().flatten().enumerate() {
            let mode = if disk.writable { "rw" } else { "ro" };
            let out = &mut self.terminal.output;
            if !disk.managed {
                out.host(format_args!("disk{n} {mode} boot\n"));
            } else if let Some(slot) = runtime.manager.volume_owner(n as u8) {
                out.host(format_args!("disk{n} {mode} managed VM {}\n", slot + 1));
            } else {
                out.host(format_args!("disk{n} {mode} managed free\n"));
            }
        }
    }
    pub(super) fn quota<A: HvArch, B: Board>(
        &mut self,
        runtime: &VmRuntime<A, B>,
        name: &str,
    ) -> Result<()> {
        let (used, limit) = runtime.manager.budget(runtime.manager.find(name)?)?;
        self.terminal
            .output
            .host(format_args!("quota {name}: used {used}; "));
        match limit {
            Some(limit) => self.terminal.output.host(format_args!("limit {limit}\n")),
            None => self
                .terminal
                .output
                .host(format_args!("board limits only\n")),
        }
        Ok(())
    }
    pub(super) fn status<A: HvArch, B: Board>(
        &mut self,
        runtime: &VmRuntime<A, B>,
        name: Option<&str>,
    ) -> Result<()> {
        let selected = name.map(|name| runtime.manager.find(name)).transpose()?;
        for vm in runtime.manager.vms() {
            if selected.is_some_and(|slot| slot != vm.slot) {
                continue;
            }
            let state = vm.lifecycle.state();
            if let Some(error) = vm.join_error() {
                self.terminal.output.host(format_args!(
                    "VM {} I/O join error={error:?}\n",
                    vm.slot + 1
                ));
            }
            if let Some(exit) = vm.terminal_exit() {
                self.terminal.output.host(format_args!(
                    "VM {} exit={:?} cause={:#x} pc={:#x} value={:#x}\n",
                    vm.slot + 1,
                    exit.kind,
                    exit.cause,
                    exit.pc,
                    exit.value
                ));
            }
            let bytes = vm.config.memory_size;
            let (size, unit) = if bytes.is_multiple_of(1 << 20) {
                (bytes >> 20, "MiB")
            } else {
                (bytes >> 10, "KiB")
            };
            self.terminal.output.host(format_args!(
                "VM id={} epoch={} name={} state={:?} vcpus={} memory={size}{unit}\n",
                vm.slot + 1,
                vm.epoch(),
                vm.config.name(),
                state,
                vm.config.vcpus
            ));
            #[cfg(feature = "selftest")]
            {
                use crate::arch::VirtualIrqChip;
                // The monitor owns epoch changes. Take one chip snapshot so an
                // owner timestamp cannot be paired with a later UART edge.
                let (files, rise) = {
                    let chip = vm.devices.irqs.lock();
                    (chip.file_stats(), chip.rx.last)
                };
                if let Some(s) = files {
                    self.terminal.output.host(format_args!(
                        "irq-file epoch={} loaded_vcpus={:#x} loads={} puts={} merges={} injected={} bound={:#x}\n",
                        vm.epoch(), s.loaded_vcpus, s.loads, s.puts, s.merges, s.injected, s.bound
                    ));
                }
                if let Some(r) = rise {
                    self.terminal.output.host(format_args!(
                        "uart-rise epoch={} seq={} byte={} ticks={}\n",
                        vm.epoch(),
                        r.seq,
                        r.byte,
                        r.ticks
                    ));
                    if let Some(o) = r.owner {
                        self.terminal.output.host(format_args!(
                            "uart-owner epoch={} seq={} vcpu={} owner={} ticks={}\n",
                            vm.epoch(),
                            r.seq,
                            o.vcpu,
                            o.owner,
                            o.ticks
                        ));
                        if let Some((send, ack, returned)) = r.paired_ack() {
                            let path = match ack.path {
                                crate::arch::NativeAckPath::GuestExit => "guest-exit",
                                crate::arch::NativeAckPath::DispatchAck => "dispatch-ack",
                            };
                            self.terminal.output.host(format_args!(
                                "uart-ack epoch={} seq={} vcpu={} owner={} path={path} send={send} ticks={} returned={returned}\n",
                                vm.epoch(), r.seq, o.vcpu, o.owner, ack.ticks
                            ));
                        }
                    }
                }
            }
            for cpu in vm.cpus() {
                let run = cpu.run_state();
                let retired = cpu.retired();
                self.terminal.output.host(format_args!(
                    "  vcpu={} owner={} state={:?} retired={} run={:?}\n",
                    cpu.index,
                    cpu.owner,
                    run.hsm_status(),
                    retired,
                    run
                ));
            }
        }
        Ok(())
    }
}
