//! Loader manifest placement relative to the selected board RAM base.
//! Ownership/Lock Ordering: immutable host-tool/board handoff constants.
//! Invariants: these locations describe host staging, never guest mappings.
pub const BOOT_CONFIG_OFFSET: u64 = 0x0f91_0000;
pub const GUEST_DTBS_OFFSET: u64 = 0x0f90_0000;
pub const CHOSEN_BOOT_CONFIG: &str = "openion,boot-config";
pub const CHOSEN_GUEST_DTBS: &str = "openion,guest-dtbs";

/// Small QEMU boards cannot address the original metadata window. Both loader
/// and launcher derive one fallback from native RAM; explicit FDT pointers win.
pub const LEGACY_RAM: u64 = 256 << 20;
pub fn metadata_offsets(ram_bytes: u64) -> (u64, u64) {
    if ram_bytes >= LEGACY_RAM {
        (GUEST_DTBS_OFFSET, BOOT_CONFIG_OFFSET)
    } else {
        let dtbs = 8 << 20; // Immediately beyond the QEMU HS linker carveout.
        (dtbs, dtbs + 64 * 1024)
    }
}
