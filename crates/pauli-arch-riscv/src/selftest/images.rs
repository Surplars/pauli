//! Opt-in architecture acceptance fixtures, absent from ordinary boot.
//! Ownership/Lock Ordering: caller owns inactive private guest backing and masked IRQs.
//! Invariants: fixture ranges do not overlap; every load/copy retains private RAM.
#[derive(Clone, Copy)]
pub struct DemoGuest {
    pub entry: usize,
    pub bytes: &'static [u8],
}

/// Stackless shared-memory rendezvous and IPI fixture for physical SMP.
pub fn parallel_guest() -> DemoGuest {
    unsafe extern "C" {
        static pauli_parallel_guest_start: u8;
        static pauli_parallel_guest_end: u8;
    }
    let start = core::ptr::addr_of!(pauli_parallel_guest_start);
    let len = core::ptr::addr_of!(pauli_parallel_guest_end) as usize - start as usize;
    DemoGuest {
        entry: 0,
        // SAFETY: the fixture owner retains the checked private RAM and immutable linker code.
        bytes: unsafe { core::slice::from_raw_parts(start, len) },
    }
}

unsafe extern "C" {
    pub(super) static pauli_timer_guest_start: u8;
    pub(super) static pauli_timer_guest_report: u8;
    pub(super) static pauli_timer_guest_end: u8;
    pub(super) static pauli_wfi_guest_start: u8;
    pub(super) static pauli_wfi_guest_wait: u8;
    pub(super) static pauli_wfi_guest_report: u8;
    pub(super) static pauli_wfi_guest_end: u8;
    pub(super) static pauli_paging_guest_start: u8;
    pub(super) static pauli_paging_guest_body: u8;
    pub(super) static pauli_paging_guest_report: u8;
    pub(super) static pauli_paging_guest_end: u8;
    pub(super) static pauli_exception_guest_start: u8;
    pub(super) static pauli_exception_guest_load: u8;
    pub(super) static pauli_exception_guest_store: u8;
    pub(super) static pauli_exception_guest_report: u8;
    pub(super) static pauli_exception_guest_handler: u8;
    pub(super) static pauli_exception_guest_target: u8;
    pub(super) static pauli_exception_guest_end: u8;
    pub(super) static pauli_exception_guest_vu_entry: u8;
    pub(super) static pauli_exception_guest_vu_spin: u8;
    pub(super) static pauli_fp_guest_start: u8;
    pub(super) static pauli_fp_guest_report: u8;
    pub(super) static pauli_fp_guest_resume_report: u8;
    pub(super) static pauli_fp_guest_vu: u8;
    pub(super) static pauli_fp_guest_spin: u8;
    pub(super) static pauli_fp_guest_end: u8;
}
pub const TIMER_OFFSET: usize = 0x200;
pub const WFI_OFFSET: usize = 0x500;
pub const PAGING_OFFSET: usize = 0x700;
pub const PAGING_AFTER_SATP: usize = PAGING_OFFSET + 4;
pub const BENCH_OFFSET: usize = 0x800;
pub const VMID_BOOT_OFFSET: usize = 0x840;
pub const VMID_BODY_OFFSET: usize = 0x860;
pub const RFENCE_BODY_OFFSET: usize = 0x880;
pub const EXCEPTION_OFFSET: usize = 0x900;
pub const FP_OFFSET: usize = 0xc00;

/// Entry/report/resume-report/VU-entry/VU-spin offsets in the fixture code page.
pub fn fp_offsets() -> [usize; 5] {
    let start = core::ptr::addr_of!(pauli_fp_guest_start) as usize;
    [
        start,
        core::ptr::addr_of!(pauli_fp_guest_report) as usize,
        core::ptr::addr_of!(pauli_fp_guest_resume_report) as usize,
        core::ptr::addr_of!(pauli_fp_guest_vu) as usize,
        core::ptr::addr_of!(pauli_fp_guest_spin) as usize,
    ]
    .map(|address| FP_OFFSET + address - start)
}

pub fn exception_vu_offsets() -> [usize; 2] {
    let start = core::ptr::addr_of!(pauli_exception_guest_start) as usize;
    [
        core::ptr::addr_of!(pauli_exception_guest_vu_entry) as usize,
        core::ptr::addr_of!(pauli_exception_guest_vu_spin) as usize,
    ]
    .map(|address| EXCEPTION_OFFSET + address - start)
}

/// Named locations in the copied exception fixture, relative to its code page.
pub fn exception_offsets() -> [usize; 5] {
    let start = core::ptr::addr_of!(pauli_exception_guest_start) as usize;
    [
        core::ptr::addr_of!(pauli_exception_guest_load) as usize,
        core::ptr::addr_of!(pauli_exception_guest_store) as usize,
        core::ptr::addr_of!(pauli_exception_guest_report) as usize,
        core::ptr::addr_of!(pauli_exception_guest_handler) as usize,
        core::ptr::addr_of!(pauli_exception_guest_target) as usize,
    ]
    .map(|address| EXCEPTION_OFFSET + address - start)
}

pub fn paging_body_offset() -> usize {
    PAGING_OFFSET + core::ptr::addr_of!(pauli_paging_guest_body) as usize
        - core::ptr::addr_of!(pauli_paging_guest_start) as usize
}
pub fn paging_report_offset() -> usize {
    PAGING_OFFSET + core::ptr::addr_of!(pauli_paging_guest_report) as usize
        - core::ptr::addr_of!(pauli_paging_guest_start) as usize
}

pub fn wfi_wait_offset() -> usize {
    WFI_OFFSET + core::ptr::addr_of!(pauli_wfi_guest_wait) as usize
        - core::ptr::addr_of!(pauli_wfi_guest_start) as usize
}

pub fn wfi_report_offset() -> usize {
    WFI_OFFSET + core::ptr::addr_of!(pauli_wfi_guest_report) as usize
        - core::ptr::addr_of!(pauli_wfi_guest_start) as usize
}

/// Offset of the BASE report ecall after guest sret, relative to its code page.
pub fn timer_report_offset() -> usize {
    TIMER_OFFSET + core::ptr::addr_of!(pauli_timer_guest_report) as usize
        - core::ptr::addr_of!(pauli_timer_guest_start) as usize
}

// Minimal little-endian RISC-V guest:
//   addi a7, zero, 1      ; legacy SBI console putchar
//   addi a0, zero, 'H'
//   ecall
const DEMO_GUEST: [u8; 12] = [
    0x93, 0x08, 0x10, 0x00, // addi a7, zero, 1
    0x13, 0x05, 0x80, 0x04, // addi a0, zero, 'H'
    0x73, 0x00, 0x00, 0x00, // ecall
];

pub fn demo_guest() -> DemoGuest {
    DemoGuest {
        entry: 0,
        bytes: &DEMO_GUEST,
    }
}
