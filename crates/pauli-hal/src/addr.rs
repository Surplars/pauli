//! Shared address/permission values, without any translation or page allocator.
//! Ownership/Lock Ordering: copyable immutable data; no locks.
//! Invariants: physical ranges and permission policy are validated by their owners.

use core::{fmt, ops};

pub const PAGE_SIZE: usize = 4096;
pub const PAGE_SHIFT: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MmError {
    OutOfMemory,
    Busy,
    AlreadyMapped,
    NotMapped,
    InvalidAlignment,
    InvalidRegion,
    InvalidPermissions,
    AccessDenied,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
#[repr(transparent)]
pub struct PhysAddr(usize);

impl PhysAddr {
    pub const ZERO: Self = Self(0);
    pub const fn new(address: usize) -> Self {
        Self(address)
    }
    pub const fn raw(self) -> usize {
        self.0
    }
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }
    pub const fn page_align(self) -> Self {
        Self(self.0 & !(PAGE_SIZE - 1))
    }
    pub const fn page_offset(self) -> usize {
        self.0 & (PAGE_SIZE - 1)
    }
    pub const fn page_number(self) -> usize {
        self.0 >> PAGE_SHIFT
    }
    pub const fn is_aligned(self, alignment: usize) -> bool {
        alignment != 0 && self.0 % alignment == 0
    }
    pub fn as_ptr<T>(self) -> *const T {
        self.0 as *const T
    }
    pub fn as_mut_ptr<T>(self) -> *mut T {
        self.0 as *mut T
    }
}

impl fmt::Debug for PhysAddr {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "PA:{:#x}", self.0)
    }
}
impl ops::Add<usize> for PhysAddr {
    type Output = Self;
    fn add(self, value: usize) -> Self {
        Self(self.0 + value)
    }
}
impl ops::Sub<usize> for PhysAddr {
    type Output = Self;
    fn sub(self, value: usize) -> Self {
        Self(self.0 - value)
    }
}
impl ops::Sub for PhysAddr {
    type Output = usize;
    fn sub(self, other: Self) -> usize {
        self.0 - other.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemPerms(u32);

impl MemPerms {
    pub const READ: Self = Self(1);
    pub const WRITE: Self = Self(2);
    pub const EXECUTE: Self = Self(4);
    pub const fn empty() -> Self {
        Self(0)
    }
    pub const fn bits(self) -> u32 {
        self.0
    }
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}
impl ops::BitOr for MemPerms {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}
impl ops::BitAnd for MemPerms {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}
