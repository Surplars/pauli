//! Declarative guest-visible board addresses, not physical host resources.
//! Ownership/Lock Ordering: immutable value data shared by consumers; no locks.
//! Invariants: alternative PLIC/APLIC layouts overlap intentionally; choose one.
//! The launcher DTB and runtime bus both consume this guest ABI.

/// A half-open guest-physical MMIO region. Device models use offsets, not GPAs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MmioRegion {
    pub base: u64,
    pub size: u64,
}

/// Virtual platform ABI independent of the physical board or interrupt routing.
/// IMSIC reserves one file page per possible vCPU; the DTB exposes actual files only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestBoardLayout {
    pub ram_base: u64,
    pub uart: MmioRegion,
    pub block: MmioRegion,
    pub net: MmioRegion,
    pub rtc: MmioRegion,
    pub plic: MmioRegion,
    pub aplic: MmioRegion,
    pub imsic: MmioRegion,
    pub uart_irq: u32,
    pub block_irq: u32,
    pub net_irq: u32,
    pub rtc_irq: u32,
}

impl GuestBoardLayout {
    /// Only the files belonging to this VM are decoded and advertised.
    pub fn imsic_files(self, vcpus: u8) -> Option<MmioRegion> {
        let size = u64::from(vcpus) * 4096;
        (size != 0 && size <= self.imsic.size).then_some(MmioRegion {
            base: self.imsic.base,
            size,
        })
    }
    /// The final context's threshold and claim/complete registers must fit.
    pub fn plic_contexts(self, vcpus: u8) -> Option<MmioRegion> {
        let last = u64::from(vcpus.checked_sub(1)?);
        (self.plic.size >= 0x20_0000 + 0x1000 * last + 8).then_some(self.plic)
    }
    /// Device apertures remain reserved even if a particular boot omits a device.
    pub fn admits_ram(self, base: u64, size: u64) -> bool {
        let Some(end) = base.checked_add(size) else {
            return false;
        };
        size != 0
            && [
                self.uart, self.block, self.net, self.rtc, self.plic, self.aplic, self.imsic,
            ]
            .iter()
            .all(|r| {
                r.size == 0
                    || r.base
                        .checked_add(r.size)
                        .is_some_and(|limit| end <= r.base || limit <= base)
            })
    }
    /// Preserves the existing guest DTB/device-bus ABI, including IRQ numbers.
    pub const DEFAULT: Self = Self {
        ram_base: 0x8000_0000,
        uart: MmioRegion {
            base: 0x1000_0000,
            size: 0x100,
        },
        block: MmioRegion {
            base: 0x1000_1000,
            size: 0x1000,
        },
        net: MmioRegion {
            base: 0x1000_2000,
            size: 0x1000,
        },
        rtc: MmioRegion {
            base: 0x0010_1000,
            size: 0x1000,
        },
        plic: MmioRegion {
            base: 0x0c00_0000,
            size: 0x0021_0000,
        },
        aplic: MmioRegion {
            base: 0x0c00_0000,
            size: 0x4000,
        },
        imsic: MmioRegion {
            base: 0x2800_0000,
            size: 4096 * crate::MAX_GUEST_VCPUS as u64,
        },
        uart_irq: 10,
        block_irq: 1,
        net_irq: 2,
        rtc_irq: 11,
    };
}

impl Default for GuestBoardLayout {
    fn default() -> Self {
        Self::DEFAULT
    }
}
