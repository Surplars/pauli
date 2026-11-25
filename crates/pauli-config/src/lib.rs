#![no_std]
#![forbid(unsafe_code)]
//! Bounded boot contract shared by the launcher and the VM owner. This is pure
//! data/validation: no allocator, CSR, board MMIO, native driver or TOML parser.
//! Fixed-width little-endian encoding never relies on Rust layout or padding.
//! Ownership/Lock Ordering: immutable input or an exclusive builder; no locks.
//! Invariants: names are private validated ASCII; wire layout is versioned.

/// Records in the v15 boot manifest, not a limit on runtime-created domains.
pub const MAX_BOOT_VMS: usize = 4;
#[deprecated(note = "use MAX_BOOT_VMS for the manifest; runtime capacity is board-specific")]
pub const MAX_VMS: usize = MAX_BOOT_VMS;
pub const MAX_DISKS: usize = 4;
/// Dense native owner indices, not firmware hart IDs or guest vCPU indices.
/// RV64 tracks resident translations and IMSIC ownership in one native word.
pub const MAX_HOST_CPUS: u8 = 64;
pub const MAX_GUEST_VCPUS: u8 = 16;
pub const MAX_GUEST_DTB: usize = 16 * 1024;
pub const NAME_MAX: usize = 24;
pub const ALIGN: u64 = 2 * 1024 * 1024;
pub const KERNEL_OFFSET: u64 = ALIGN;
pub const MAX_KERNEL_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_INITRD_BYTES: u64 = 128 * 1024 * 1024;
pub const MIN_RAM_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_RAM_BYTES: u64 = 1024 * 1024 * 1024;
pub const FIRMWARE_PAGE: u64 = 4096;
/// Bounded page-granular mapping fits the existing private G-stage arena.
pub const MAX_FIRMWARE_RAM: u64 = ALIGN;
/// Firmware images have an RX prefix, optional R-only pages, then RW/NX RAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirmwareLayout {
    pub rx_pages: u16,
    pub ro_pages: u16,
}
pub const HEADER: usize = 64;
pub const RECORD: usize = 136;
pub const DISK_RECORD: usize = 16;
pub const DISKS_OFFSET: usize = HEADER + RECORD * MAX_BOOT_VMS;
pub const WIRE_SIZE: usize = DISKS_OFFSET + DISK_RECORD * MAX_DISKS;
const MAGIC: &[u8; 8] = b"IONGST15";

/// Boot-time backend slot, not a driver enumeration index or guest address.
/// Board code supplies the slot's transport; every VM keeps private queues.
#[derive(Clone, Copy, Debug)]
pub struct DiskConfig {
    pub sectors: u64,
    pub writable: bool,
    /// Explicitly withheld from boot VMs for exclusive assignment at runtime.
    pub managed: bool,
}

/// Immutable host template sources, not guest RAM addresses or host paths.
/// Identical sources may be shared; partial/cross-kind aliases are rejected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VmImage {
    pub kernel_source: u64,
    pub kernel_size: u64,
    pub kernel_memory_size: u64,
    pub initrd_source: u64,
    pub initrd_size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Version,
    Reserved,
    Count,
    Name,
    Memory,
    Image,
    Dtb,
    Quantum,
    Vcpus,
    Affinity,
    Disk,
    Network,
}

/// Physical ownership is resolved once after firmware identifies the boot hart.
/// Workers spreads over non-management harts and deliberately allows overcommit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuAffinity {
    Automatic,
    Physical([u8; MAX_GUEST_VCPUS as usize]),
    Workers,
}

#[derive(Clone, Copy, Debug)]
pub struct VmConfig {
    name: [u8; NAME_MAX],
    pub guest_base: u64,
    pub host_base: u64,
    pub memory_size: u64,
    pub dtb_offset: u64,
    pub dtb_size: u64,
    pub quantum_us: u64,
    pub autostart: bool,
    pub disk: Option<u8>,
    pub writable: bool,
    /// Explicit reciprocal point-to-point partner slot, never a guest address.
    pub net_peer: Option<u8>,
    /// Exclusive native NIC. The board currently has one fixed DMA pool; it
    /// cannot be shared with another VM or combined with a paired endpoint.
    pub net_uplink: bool,
    /// Install media must not be re-entered after the installer requests reset.
    /// Stop at the manager instead; ordinary guests reload their current image.
    pub stop_on_reset: bool,
    /// Opt-in software APLIC/IMSIC guest topology. PLIC remains the default.
    pub aia: bool,
    pub vcpus: u8,
    /// Explicit arrays retain logical native owner indices. Workers excludes the actual
    /// boot/management hart. Unused array entries must remain zero.
    pub cpu_affinity: CpuAffinity,
    /// Reserve the distinct, explicitly pinned owners for this VM at boot.
    pub exclusive_cpus: bool,
    pub image: VmImage,
    pub firmware: Option<FirmwareLayout>,
}
#[derive(Clone, Copy, Debug)]
pub struct BootConfig {
    pub template_base: u64,
    pub template_size: u64,
    pub dtb_size: u64,
    pub management: bool,
    pub smoke_payload: bool,
    pub disks: [Option<DiskConfig>; MAX_DISKS],
    pub vms: [Option<VmConfig>; MAX_BOOT_VMS],
}

pub mod guest_dtb;
mod layout;
pub mod linux_image;
pub mod loader;
mod validation;
mod vm;
mod wire;
pub use layout::{GuestBoardLayout, GuestIrqConfig, GuestIrqController, ImsicIds, MmioRegion};

#[cfg(test)]
#[macro_use]
extern crate std;
#[cfg(test)]
mod tests;
pub use validation::overlaps;
