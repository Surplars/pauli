//! Single VM ownership: immutable topology and explicit joined lifecycle.
//! Ownership/Lock Ordering: manager -> vCPU metadata -> devices -> link/driver.
//! No business lock is held across guest entry; release every lock before logs.
//! Invariants: terminal state alone is NOT a retirement or device-join proof.
//! Lifecycle: Joined -> Ready -> Stopping -> Joined; fault/reset also join first.
mod construction;
pub mod devices;
mod diagnostics;
mod image;
mod irq_bank;
pub(crate) use image::validate_embedded;
pub mod lifecycle;
mod management;
pub mod slot;
use crate::{
    arch::HvArch,
    memory::{ArenaSlice, RamImage},
};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use devices::VmDevices;
pub(crate) use diagnostics::{ActivationFault, KickFault, ManagementFault, VmFaults};
pub use lifecycle::State;
use pauli_config::VmConfig;
use slot::VcpuSlot;

pub struct Vm<A: HvArch> {
    pub config: VmConfig,
    pub slot: usize,
    pub lifecycle: lifecycle::Lifecycle,
    pub(crate) notice: lifecycle::Notice,
    pub(crate) space: A::Space,
    pub devices: VmDevices<A::Irq>,
    cpus: ArenaSlice<VcpuSlot<A::Vcpu>>,
    pub quantum: u64,
    pub(crate) image: RamImage,
    pub(crate) allocation: Option<crate::memory::pool::Receipt>,
    pub(crate) sponsor: Option<pauli_control::VmId>,
    pub(crate) quota: pauli_hal::sync::SpinLock<Option<crate::manager::Budget>>,
    boot: crate::arch::VcpuBoot,
    pub(crate) epoch: AtomicU32,
    pub(crate) started: AtomicBool,
    diagnostics: diagnostics::Diagnostics,
    terminal: pauli_hal::sync::SpinLock<Option<crate::arch::VcpuExit>>,
}
impl<A: HvArch> Vm<A> {
    pub fn epoch(&self) -> u32 {
        self.epoch.load(Ordering::Acquire)
    }
    pub(crate) fn guest_sstc(&self) -> bool {
        self.boot.sstc
    }
    pub fn cpu(&self, index: usize) -> Option<&VcpuSlot<A::Vcpu>> {
        self.cpus.get(index)
    }
    pub fn cpus(&self) -> &[VcpuSlot<A::Vcpu>] {
        &self.cpus
    }
    /// Board-owned arena bytes for this topology, not for every supported CPU.
    pub fn vcpu_arena_bytes(count: u8) -> crate::Result<usize> {
        ArenaSlice::<VcpuSlot<A::Vcpu>>::budget(usize::from(count))
    }
    pub fn retired(&self) -> bool {
        self.cpus.iter().all(|cpu| cpu.retired())
    }
    pub fn terminal_exit(&self) -> Option<crate::arch::VcpuExit> {
        *self.terminal.lock()
    }
    pub(crate) fn remember_exit(&self, exit: crate::arch::VcpuExit) {
        *self.terminal.lock() = Some(exit);
    }
}
