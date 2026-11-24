//! Board-only physical resources; the VMM knows no native MMIO addresses.
//! Ownership/Lock Ordering: boot manager consumes resources once, before workers.
//! Invariants: images/arenas are reserved and disjoint from every guest assignment.
use crate::{
    HvError, Result,
    memory::{Arena, GuestRam},
};
use pauli_config::{BootConfig, GuestBoardLayout, VmConfig};
use pauli_hal::device::{HostBlockDevice, HostNetDevice, HostRtcDevice};
/// Embedded payloads are explicitly supplied by a trusted board, never
/// selected by the wire manifest. No Linux DTB/initrd or writable code page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Payload {
    Linux,
    /// A validated boot record with page-granular ELF permissions and a DTB.
    Firmware(pauli_config::FirmwareLayout),
    Embedded,
}

#[derive(Clone, Copy, Debug)]
pub struct BoardInfo {
    pub ram_base: usize,
    pub ram_size: usize,
    pub kernel_end: usize,
    pub timebase_hz: u64,
    pub host_cpus: usize,
    pub boot_cpu: usize,
}
pub struct BootImages {
    pub kernel: &'static [u8],
    pub initrd: &'static [u8],
    pub dtb: &'static [u8],
}
pub struct VmResources {
    pub ram: GuestRam,
    pub arena: Arena,
    pub images: BootImages,
}
pub trait Board: Send + Sync + 'static {
    /// Native console RX wakes the boot hart; a diskless, console-only board
    /// can defer its fallback polling for up to one millisecond.
    const IRQ_DRIVEN_CONSOLE: bool = false;
    /// Optional extended slice (microseconds) when a single Ready vCPU is the
    /// sole active lane on its physical hart. Defaults to None (exact vm.quantum).
    const UNCONTENDED_QUANTUM_US: Option<u64> = None;

    fn info(&self) -> BoardInfo;
    fn layout(&self) -> GuestBoardLayout {
        GuestBoardLayout::DEFAULT
    }
    fn payload(&self) -> Payload {
        Payload::Linux
    }
    /// Dedicate native UART bytes to boot slot 0 instead of opening the host
    /// command line. This is board policy, not a grant to manage other VMs.
    fn direct_linux_console(&self) -> bool {
        false
    }
    fn load_boot_config(&self) -> Result<&'static BootConfig>;
    /// Optional native boot policy: this immutable manager slot may inspect,
    /// start and stop one immutable service slot. Both indices are zero-based.
    /// No guest-provided name, DTB or RPC can select the grantee or its target.
    fn boot_service_for(&self, _manager_slot: usize) -> Option<usize> {
        None
    }
    /// Optional residual RAM after firmware, native DMA and boot guests are reserved.
    fn domain_pool(&self) -> Option<&crate::memory::pool::DomainPool> {
        None
    }
    /// Consume one private assignment once. Failure never releases live memory.
    fn take_resources(&self, slot: usize, config: &VmConfig) -> Result<VmResources>;
    /// Distinct slots must name disjoint storage for the host lifetime, including
    /// any partitions or files behind a driver. Shared boot readers use one slot.
    fn resolve_block(&self, _slot: u8) -> Result<&'static dyn HostBlockDevice> {
        Err(HvError::Unsupported)
    }
    fn resolve_uplink_net(&self) -> Result<&'static dyn HostNetDevice> {
        Err(HvError::Unsupported)
    }
    fn rtc(&self) -> Option<&'static dyn HostRtcDevice> {
        None
    }
    fn pop_console_rx(&self) -> Option<u8>;
    /// Positive hint that native IRQ handling already queued input. Called
    /// with local IRQs masked; never consume data or poll hardware here.
    /// A false result is not a guarantee that the monitor can sleep indefinitely.
    fn console_rx_queued(&self) -> bool {
        false
    }
    /// Bounded native polling/IRQ completion. It may signal CompletionEvents,
    /// but never enters guest execution or calls back into locked VM devices.
    fn service_devices(&self, now: u64);
    fn shutdown(&self) -> !;
}
