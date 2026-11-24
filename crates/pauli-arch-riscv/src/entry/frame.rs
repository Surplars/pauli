//! Typed HS envelope and resident CSR bank used solely for assembly layout operands.
//! Ownership/Lock Ordering: one IRQ-masked CPU owns the native frame/bank.
//! Invariants: all guest offsets come from offset_of!, and native frames stay 16-byte aligned.

use crate::fp::FloatState;
#[repr(C)]
#[derive(Default)]
pub(crate) struct HostBank {
    pub hedeleg: usize,
    pub hideleg: usize,
    pub hgatp: usize,
    pub hvip: usize,
    pub hcounteren: usize,
    pub henvcfg: usize,
    pub vsstatus: usize,
    pub vsatp: usize,
    pub vstvec: usize,
    pub vsscratch: usize,
    pub vsepc: usize,
    pub vscause: usize,
    pub vstval: usize,
    pub htimedelta: usize,
    pub hie: usize,
}
#[repr(C, align(16))]
pub(super) struct HostFrame {
    pub registers: [usize; 16],
    pub sstatus: usize,
    pub stvec: usize,
    pub sscratch: usize,
    pub sie: usize,
    pub sepc: usize,
    pub hstatus: usize,
    pub bank: HostBank,
    pub scause: usize,
    pub stval: usize,
    pub htval: usize,
    pub htinst: usize,
    pub reserved0: usize,
    pub senvcfg: usize,
    pub scounteren: usize,
    pub reserved1: usize,
    pub mode: usize,
    pub reserved2: [usize; 2],
    pub fp: FloatState,
    pub padding: usize,
}
const _: () = {
    assert!(core::mem::size_of::<HostFrame>() % 16 == 0);
    assert!(core::mem::size_of::<HostBank>() == 15 * core::mem::size_of::<usize>());
};
