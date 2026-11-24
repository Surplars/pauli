//! AIA CSR catalog, including RV32 high halves and optional H/VS state.
//!
//! All access is unsafe: compiling a backend does not prove hardware support,
//! firmware permission, or ownership. Prefer the interrupt-safe indirect window
//! in the parent module for host *iselect/*ireg operations. Hypervisor callers
//! must also serialize guest selection (hstatus.VGEIN) across VS-file accesses.
//! These H/VS accessors do not allocate or assign guest interrupt files.

//! Ownership/Lock Ordering: caller owns the discovered CSR bank and excludes context/IRQ conflicts.
//! Invariants: unsafe access never implies hardware availability or guest-file assignment.
macro_rules! read_csr {
    ($name:ident, $address:literal) => {
        pub mod $name {
            pub const ADDRESS: u16 = $address;
            /// Read without changing pending state.
            /// # Safety
            /// The CSR must exist and be accessible at the current privilege.
            #[inline]
            pub unsafe fn read() -> usize {
                let value;
                // SAFETY: the unsafe accessor contract grants this discovered, accessible, exclusively owned CSR bank.
                unsafe { core::arch::asm!("csrr {value}, {csr}", value = out(reg) value,
                    csr = const ADDRESS, options(nostack)); }
                value
            }
        }
    };
}

macro_rules! rw_csr {
    ($name:ident, $address:literal) => {
        pub mod $name {
            pub const ADDRESS: u16 = $address;
            /// # Safety
            /// The CSR must exist and be accessible at the current privilege.
            #[inline]
            pub unsafe fn read() -> usize {
                let value;
                // SAFETY: the unsafe accessor contract grants this discovered, accessible, exclusively owned CSR bank.
                unsafe { core::arch::asm!("csrr {value}, {csr}", value = out(reg) value,
                    csr = const ADDRESS, options(nostack)); }
                value
            }
            /// Write WARL state. Read back if the accepted value matters.
            /// # Safety
            /// The caller owns the CSR state and has excluded conflicting
            /// interrupt/context-switch access. The CSR must be accessible.
            #[inline]
            pub unsafe fn write(value: usize) {
                // SAFETY: the unsafe accessor contract grants this discovered, accessible, exclusively owned CSR bank.
                unsafe { core::arch::asm!("csrw {csr}, {value}", csr = const ADDRESS,
                    value = in(reg) value, options(nostack)); }
            }
            /// Atomically read and replace owned writable CSR state.
            /// # Safety
            /// Same accessible, exclusively owned bank as write().
            #[inline]
            pub unsafe fn swap(value: usize) -> usize {
                let old;
                // SAFETY: the unsafe accessor contract grants this discovered, accessible, exclusively owned CSR bank.
                unsafe { core::arch::asm!("csrrw {old}, {csr}, {value}", csr = const ADDRESS,
                    old = out(reg) old, value = in(reg) value, options(nostack)); }
                old
            }
            /// # Safety
            /// Same writable bank as write(); selected registers must permit
            /// CSR set-bit semantics (not acknowledge-on-write registers).
            #[inline]
            pub unsafe fn set(bits: usize) {
                // SAFETY: the unsafe accessor contract grants this discovered, accessible, exclusively owned CSR bank.
                unsafe { core::arch::asm!("csrs {csr}, {bits}", csr = const ADDRESS,
                    bits = in(reg) bits, options(nostack)); }
            }
        }
    };
}

macro_rules! claim_csr {
    ($name:ident, $address:literal) => {
        pub mod $name {
            pub const ADDRESS: u16 = $address;
            /// Atomically return and acknowledge the top external interrupt.
            /// No plain write accessor is exposed: a delayed separate write
            /// can clear a newer higher-priority interrupt instead.
            /// # Safety
            /// The caller owns the selected, accessible IMSIC file.
            #[inline]
            pub unsafe fn claim() -> usize {
                let value;
                // SAFETY: the unsafe accessor contract grants this discovered, accessible, exclusively owned CSR bank.
                unsafe { core::arch::asm!("csrrw {value}, {csr}, zero", value = out(reg) value,
                    csr = const ADDRESS, options(nostack)); }
                value
            }
        }
    };
}

pub mod supervisor {
    rw_csr!(siselect, 0x150);
    rw_csr!(sireg, 0x151);
    claim_csr!(stopei, 0x15c);
    read_csr!(stopi, 0xdb0);
    rw_csr!(sie, 0x104);
    rw_csr!(sip, 0x144);
    #[cfg(target_pointer_width = "32")]
    pub mod high {
        rw_csr!(sieh, 0x114);
        rw_csr!(siph, 0x154);
    }
}

pub mod hypervisor {
    rw_csr!(hstatus, 0x600);
    rw_csr!(hgeie, 0x607);
    rw_csr!(hideleg, 0x603);
    rw_csr!(hvien, 0x608);
    rw_csr!(hvictl, 0x609);
    rw_csr!(hvip, 0x645);
    rw_csr!(hviprio1, 0x646);
    rw_csr!(hviprio2, 0x647);
    rw_csr!(vsiselect, 0x250);
    rw_csr!(vsireg, 0x251);
    claim_csr!(vstopei, 0x25c);
    read_csr!(vstopi, 0xeb0);
    rw_csr!(vsie, 0x204);
    rw_csr!(vsip, 0x244);
    #[cfg(target_pointer_width = "32")]
    pub mod high {
        rw_csr!(hidelegh, 0x613);
        rw_csr!(hvienh, 0x618);
        rw_csr!(hviph, 0x655);
        rw_csr!(hviprio1h, 0x656);
        rw_csr!(hviprio2h, 0x657);
        rw_csr!(vsieh, 0x214);
        rw_csr!(vsiph, 0x254);
    }
}
