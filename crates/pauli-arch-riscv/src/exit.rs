//! Trusted HS exit snapshot; scalars are not guest-provided instruction authority.
//! Ownership/Lock Ordering: returned by value to the off-CPU owner; no locks.
//! Invariants: htval is GPA>>2; PC is sepc, distinct from the guest's vsepc.

#[derive(Clone, Copy, Debug)]
pub struct VmExit {
    pub cause: usize,
    pub value: usize,
    pub gpa_shifted: usize,
    pub instruction: usize,
    pub pc: usize,
    pub from_guest: bool,
}
impl VmExit {
    pub const VS_ECALL: usize = 10;
    pub const SUPERVISOR_SOFTWARE: usize = (1usize << 63) | 1;
    pub const SUPERVISOR_TIMER: usize = (1usize << 63) | 5;
    pub const SUPERVISOR_EXTERNAL: usize = (1usize << 63) | 9;
    pub const VIRTUAL_INSTRUCTION: usize = 22;
    pub const INSTRUCTION_PAGE_FAULT: usize = 12;
    pub const LOAD_PAGE_FAULT: usize = 13;
    pub const STORE_PAGE_FAULT: usize = 15;
    pub const GUEST_LOAD_PAGE_FAULT: usize = 21;
    pub const GUEST_STORE_PAGE_FAULT: usize = 23;
    pub const GUEST_INSTRUCTION_PAGE_FAULT: usize = 20;
}
