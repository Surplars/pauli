//! Resident management owner: console, permissions and lifecycle notifications.
//! Ownership/Lock Ordering: sole management writer; never hold queues across effects.
//! Invariants: guest management goes through authorized scalar RPC, never direct mutation.
use crate::{
    HvError, Result, arch::HvArch, board::Board, runtime::VmRuntime, vm::lifecycle::State,
};
use pauli_control::{Caller, Operation, Request, Service, VmId};
pub(crate) mod backend;
pub mod commands;
pub mod console_loop;
mod dispatcher;
#[cfg(feature = "guest-control")]
pub(crate) mod rpc;
mod status_fmt;
use backend::Target;
use commands::Command;
use console_loop::{DETACH, Output, Terminal};
pub struct Monitor {
    pub terminal: Terminal,
    service: Service,
    shutdown: bool,
}
impl Monitor {
    pub(crate) fn close<A: HvArch, B: Board>(&mut self, runtime: &VmRuntime<A, B>) {
        if self.shutdown {
            return;
        }
        self.shutdown = true;
        for vm in runtime.manager.vms() {
            vm.host_stop();
        }
    }
    pub const fn new() -> Self {
        Self {
            terminal: Terminal::new(),
            service: Service::new(),
            shutdown: false,
        }
    }
    pub fn welcome<A: HvArch, B: Board>(&mut self, runtime: &VmRuntime<A, B>) {
        if runtime.board.direct_linux_console() {
            self.terminal.output = Output::Primary;
        } else {
            self.terminal.output.host(format_args!(
                "Pauli hypervisor host monitor; help for commands\nhv> "
            ));
        }
        for (slot, error) in runtime.manager.errors.iter().enumerate() {
            if let Some(error) = error {
                self.terminal.output.host(format_args!(
                    "VM {} provisioning failed: {error:?}\n",
                    slot + 1
                ));
            }
        }
        if !self.terminal.output.direct() && !runtime.manager.config.management {
            if let Ok(vm) = runtime.manager.vm(0) {
                if vm.lifecycle.stopping() {
                    let _ = runtime.manager.start(runtime.board, 0);
                }
                if !vm.lifecycle.stopping() {
                    self.terminal.attach(0, vm.epoch());
                }
            }
        }
    }
    pub fn poll<A: HvArch, B: Board>(&mut self, runtime: &VmRuntime<A, B>) {
        runtime.manager.service(runtime.board);
        let any_ready = self.notifications(runtime);
        #[cfg(feature = "guest-control")]
        rpc::pump(runtime, &mut self.service, !self.shutdown);
        if self.terminal.output.direct() {
            Self::primary_input(runtime);
        } else {
            self.monitor_input(runtime);
        }
        if !runtime.manager.config.management
            && runtime.manager.config.smoke_payload
            && !any_ready
            && runtime.manager.vms().all(|vm| vm.lifecycle.joined())
        {
            self.close(runtime);
        }
        for vm in runtime.manager.vms() {
            // Drain even unselected service VMs so their virtual TX can progress.
            let mut bytes = [0; 256];
            let count = vm.devices.io.lock().console.drain(&mut bytes);
            if count != 0 {
                self.terminal
                    .output
                    .guest(vm.slot, vm.epoch(), &bytes[..count]);
            }
        }
        if self.shutdown && runtime.manager.vms().all(|vm| vm.lifecycle.joined()) {
            runtime.board.shutdown();
        }
    }
    /// Fixed native route, not a privileged command stream. Preserve control bytes
    /// and leave bursts in the board RX ring until Linux frees virtual FIFO space.
    fn primary_input<A: HvArch, B: Board>(runtime: &VmRuntime<A, B>) {
        use crate::arch::ArchTimer;
        let vm = runtime.manager.vm(0).ok();
        for _ in 0..32 {
            let live = vm.as_ref().filter(|vm| vm.lifecycle.can_enter());
            if live.is_some_and(|vm| !vm.devices.io.lock().console.uart.rx_has_room()) {
                break;
            }
            let Some(byte) = runtime.board.pop_console_rx() else {
                break;
            };
            if let Some(vm) = live {
                vm.devices.receive_console(byte, A::Timer::now);
                vm.deliver_wakes();
            }
            // A stopped primary has no shell fallback; discard rather than
            // interpreting the byte as a command or delivering it to a peer.
        }
    }
    fn monitor_input<A: HvArch, B: Board>(&mut self, runtime: &VmRuntime<A, B>) {
        for _ in 0..32 {
            let Some(byte) = runtime.board.pop_console_rx() else {
                break;
            };
            if byte == DETACH && self.terminal.attached.is_some() {
                self.terminal.detach();
                continue;
            }
            if let Some((slot, epoch)) = self.terminal.attached {
                let request = Request {
                    vm: VmId(slot as u32 + 1),
                    epoch,
                    operation: Operation::Input(byte),
                };
                let _ = self
                    .service
                    .execute(Caller::Host, request, &mut Target(runtime));
            } else if self.terminal.push(byte) {
                self.line(runtime);
            }
        }
    }
    fn line<A: HvArch, B: Board>(&mut self, runtime: &VmRuntime<A, B>) {
        let mut bytes = [0; 160];
        let result = match self.terminal.line(&mut bytes) {
            Some(len) => core::str::from_utf8(&bytes[..len])
                .map_err(|_| HvError::InvalidCommand)
                .and_then(commands::parse)
                .and_then(|command| self.execute(runtime, command)),
            None => Err(HvError::InvalidCommand),
        };
        self.terminal.reset_line();
        if let Err(error) = result {
            self.terminal.output.host(format_args!("vm: {:?}\n", error));
        }
        if self.terminal.attached.is_none() {
            self.terminal.output.host(format_args!("hv> "));
        }
    }
    fn operation<A: HvArch, B: Board>(
        &mut self,
        runtime: &VmRuntime<A, B>,
        name: &str,
        operation: Operation,
    ) -> Result<usize> {
        let slot = runtime.manager.find(name)?;
        // Do not hold a reader pin across destroy; the returned epoch is the handle.
        let epoch = runtime.manager.vm(slot)?.epoch();
        let request = Request {
            vm: VmId(slot as u32 + 1),
            epoch,
            operation,
        };
        let reply = self
            .service
            .execute(Caller::Host, request, &mut Target(runtime))
            .map_err(|error| match error {
                pauli_control::Error::Busy => HvError::Busy,
                pauli_control::Error::NoSpace => HvError::Exhausted,
                pauli_control::Error::Unsupported => HvError::Unsupported,
                pauli_control::Error::Denied => HvError::Permission,
                pauli_control::Error::Invalid => HvError::InvalidConfig,
                _ => HvError::InvalidState,
            })?;
        Ok(reply.info.vm.0 as usize - 1)
    }
}
