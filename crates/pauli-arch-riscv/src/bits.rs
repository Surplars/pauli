//! Named RISC-V HS/VS control bits shared by Rust and transition assembly.
//! Ownership/Lock Ordering: immutable architecture constants; no state or locks.
//! Invariants: delegation admits guest exceptions only, counters expose TIME only.

pub const SSTATUS_SIE: usize = 1 << 1;
pub const SSTATUS_SPP: usize = 1 << 8;
pub const SSTATUS_VS: usize = 3 << 9;
pub const SSTATUS_FS: usize = 3 << 13;
pub const HSTATUS_VSBE: usize = 1 << 5;
pub const HSTATUS_SPV: usize = 1 << 7;
pub const HSTATUS_SPVP: usize = 1 << 8;
pub const HSTATUS_VTW: usize = 1 << 21;
pub const HSTATUS_VGEIN: usize = 0x3f << 12;
pub const HSTATUS_VGEIN_SHIFT: usize = 12;
pub const VSSIP: usize = 1 << 2;
pub const VSTIP: usize = 1 << 6;
pub const VSEIP: usize = 1 << 10;
pub const VIRTUAL_PENDING: usize = VSSIP | VSTIP | VSEIP;
pub const SSIP: usize = 1 << 1;
pub const STIP: usize = 1 << 5;
pub const SEIP: usize = 1 << 9;
pub const HOST_GUEST_INTERRUPTS: usize = SSIP | STIP;
pub const COUNTER_TIME: usize = 1 << 1;
pub const VMID_SHIFT: usize = 44;
pub const VMID_BITS: usize = 14;
pub const VMID_MASK: usize = (1 << VMID_BITS) - 1;
pub const HGATP_SV39X4: usize = 8 << 60;
pub const HENVCFG_STCE: usize = 1 << 63;
pub const CSR_STIMECMP: u16 = 0x14d;
pub const CSR_VSTIMECMP: u16 = 0x24d;
