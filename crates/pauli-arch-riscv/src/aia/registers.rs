//! AIA virtual register ABI; offsets are relative to a VM-assigned region.
//! Ownership/Lock Ordering: immutable constants, no state or locks.
//! Invariants: these offsets grant no physical CSR, MSI target or MMIO authority.
pub const SIREG: u16 = 0x151;
pub const STOPEI: u16 = 0x15c;
pub const PRIORITY_FIRST: usize = 0x30;
pub const PRIORITY_LAST: usize = 0x3f;
pub const PRIORITY_LAST_EVEN: usize = 0x3e;
pub const EIDELIVERY: usize = 0x70;
pub const EITHRESHOLD: usize = 0x72;
pub const EIP: usize = 0x80;
pub const EIP_LAST: usize = 0xbf;
pub const EIE: usize = 0xc0;
pub const EIE_LAST: usize = 0xff;
pub const SELECTOR_WORD_MASK: usize = 0x3f;
pub const APLIC_SIZE: usize = 0x4000;
pub const SOURCE_FIRST: usize = 4;
pub const SOURCE_LAST: usize = 0x7c;
pub const SETIP: usize = 0x1c00;
pub const SETIPNUM: usize = 0x1cdc;
pub const CLRIP: usize = 0x1d00;
pub const CLRIPNUM: usize = 0x1ddc;
pub const SETIE: usize = 0x1e00;
pub const SETIENUM: usize = 0x1edc;
pub const CLRIE: usize = 0x1f00;
pub const CLRIENUM: usize = 0x1fdc;
pub const SETIPNUM_LE: usize = 0x2000;
pub const SETIPNUM_BE: usize = 0x2004;
pub const GENMSI: usize = 0x3000;
pub const TARGET_FIRST: usize = 0x3004;
pub const TARGET_LAST: usize = 0x307c;
pub const MSI_FILE_SIZE: usize = 4096;
