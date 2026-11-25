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

const IMSIC_FILE_BYTES: u64 = 4096;
const APLIC_BYTES: u64 = 0x4000;

/// Highest implemented IMSIC interrupt identity. Identity zero is reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ImsicIds {
    Ids127 = 127,
    Ids255 = 255,
}

impl ImsicIds {
    pub const fn from_count(count: usize) -> Option<Self> {
        match count {
            127 => Some(Self::Ids127),
            255 => Some(Self::Ids255),
            _ => None,
        }
    }

    pub const fn max_id(self) -> usize {
        self as usize
    }
}

/// Guest-visible controller apertures after selecting exactly one interrupt topology.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestIrqController {
    Legacy {
        region: MmioRegion,
    },
    Advanced {
        wired: MmioRegion,
        message: MmioRegion,
        ids: ImsicIds,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestIrqConfig {
    pub vcpus: u8,
    pub controller: GuestIrqController,
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
    pub imsic_ids: ImsicIds,
    pub uart_irq: u32,
    pub block_irq: u32,
    pub net_irq: u32,
    pub rtc_irq: u32,
    pub control_irq: u32,
}

impl GuestBoardLayout {
    /// Resolve the apertures shared by the device tree, MMIO bus and IRQ chip.
    pub fn irq_config(self, vcpus: u8, advanced: bool) -> Result<GuestIrqConfig, crate::Error> {
        use crate::Error;
        if !(1..=crate::MAX_GUEST_VCPUS).contains(&vcpus) {
            return Err(Error::Vcpus);
        }
        let sources = [
            self.uart_irq,
            self.block_irq,
            self.net_irq,
            self.rtc_irq,
            self.control_irq,
        ];
        for (i, source) in sources.iter().enumerate() {
            if !(1..32).contains(source) || sources[..i].contains(source) {
                return Err(Error::Dtb);
            }
        }
        let controller = if advanced {
            let wired = self.aplic;
            let message = self.imsic_files(vcpus).ok_or(Error::Dtb)?;
            let wired_end = wired.base.checked_add(wired.size).ok_or(Error::Dtb)?;
            let files_end = message.base.checked_add(message.size).ok_or(Error::Dtb)?;
            if wired.size < APLIC_BYTES
                || wired.base % 4 != 0
                || message.base % IMSIC_FILE_BYTES != 0
                || (wired.base < files_end && message.base < wired_end)
            {
                return Err(Error::Dtb);
            }
            GuestIrqController::Advanced {
                wired,
                message,
                ids: self.imsic_ids,
            }
        } else {
            let region = self.plic_contexts(vcpus).ok_or(Error::Dtb)?;
            if region.base % 4 != 0 || region.base.checked_add(region.size).is_none() {
                return Err(Error::Dtb);
            }
            GuestIrqController::Legacy { region }
        };
        Ok(GuestIrqConfig { vcpus, controller })
    }
    /// Only the files belonging to this VM are decoded and advertised.
    pub fn imsic_files(self, vcpus: u8) -> Option<MmioRegion> {
        let size = u64::from(vcpus) * IMSIC_FILE_BYTES;
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
            size: APLIC_BYTES,
        },
        imsic: MmioRegion {
            base: 0x2800_0000,
            size: IMSIC_FILE_BYTES * crate::MAX_GUEST_VCPUS as u64,
        },
        imsic_ids: ImsicIds::Ids255,
        uart_irq: 10,
        block_irq: 1,
        net_irq: 2,
        rtc_irq: 11,
        control_irq: 12,
    };
}

impl Default for GuestBoardLayout {
    fn default() -> Self {
        Self::DEFAULT
    }
}
