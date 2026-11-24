//! Split-ring little-endian wire types, shared by native block and net drivers.
//! Ownership/Lock Ordering: native queue lock serializes CPU access.
//! Invariants: DMA storage is pinned/aligned; volatile accesses respect ownership fences.

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VirtqDesc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}
#[repr(C)]
pub struct VirtqAvail<const N: usize> {
    pub flags: u16,
    pub idx: u16,
    pub ring: [u16; N],
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VirtqUsedElem {
    pub id: u32,
    pub len: u32,
}
#[repr(C)]
pub struct VirtqUsed<const N: usize> {
    pub flags: u16,
    pub idx: u16,
    pub ring: [VirtqUsedElem; N],
}

const _: () = assert!(core::mem::size_of::<VirtqDesc>() == 16);
