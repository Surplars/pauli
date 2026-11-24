//! One software S-domain, MSI delivery only, 31 wired sources. Target hart
//! indices name files in this VM, never physical harts or host MSI addresses.
//! Ownership/Lock Ordering: one VM device owner, then the file bank.
//! Invariants: only validated VM-local target files and IDs can receive MSIs.
use super::imsic::{File, MAX_ID};
use super::registers::{
    CLRIE, CLRIENUM, CLRIP, CLRIPNUM, GENMSI, SETIE, SETIENUM, SETIP, SETIPNUM, SETIPNUM_BE,
    SETIPNUM_LE, SOURCE_FIRST, SOURCE_LAST, TARGET_FIRST, TARGET_LAST,
};
pub struct Aplic {
    modes: [u8; 32],
    targets: [u32; 32],
    levels: u32,
    rectified: u32,
    pending: u32,
    enabled: u32,
    delivery: bool,
    generated: u32,
    pub msis: u64,
    pub rearms: u64,
}
impl Aplic {
    pub const fn new() -> Self {
        Self {
            modes: [0; 32],
            targets: [0; 32],
            levels: 0,
            rectified: 0,
            pending: 0,
            enabled: 0,
            delivery: false,
            generated: 0,
            msis: 0,
            rearms: 0,
        }
    }
    fn active(&self) -> u32 {
        (1..32).fold(0, |mask, source| {
            mask | (u32::from(self.modes[source] != 0) << source)
        })
    }
    pub fn set_level(&mut self, source: u32, high: bool) {
        assert!((1..32).contains(&source)); // Trusted device wiring, not guest input.
        let bit = 1 << source;
        if high {
            self.levels |= bit;
        } else {
            self.levels &= !bit;
        }
        self.rectify(source as usize);
    }
    fn rectify(&mut self, source: usize) {
        let bit = 1 << source;
        let mode = self.modes[source];
        let old = self.rectified & bit != 0;
        let high = match mode {
            4 | 6 => self.levels & bit != 0,
            5 | 7 => self.levels & bit == 0,
            _ => false,
        };
        if high {
            self.rectified |= bit;
            if !old {
                self.pending |= bit;
            }
        } else {
            self.rectified &= !bit;
            if mode >= 6 {
                self.pending &= !bit;
            }
        }
    }
    fn set_pending(&mut self, mask: u32) {
        let mut remaining = mask & self.active();
        while remaining != 0 {
            let source = remaining.trailing_zeros() as usize;
            let bit = 1 << source;
            remaining &= !bit;
            if self.modes[source] < 6 || self.rectified & bit != 0 {
                self.pending |= bit;
            }
        }
    }
    fn number(value: u32) -> u32 {
        if (1..32).contains(&value) {
            1 << value
        } else {
            0
        }
    }
    fn target(value: u32, cpus: usize) -> u32 {
        if (value >> 18) as usize >= cpus || value & 0x3f000 != 0 || value & 0x7ff > MAX_ID as u32 {
            0
        } else {
            value & 0xfffc_07ff
        }
    }
    pub fn read(&self, offset: usize) -> u32 {
        match offset {
            0 => 0x8000_0004 | (u32::from(self.delivery) << 8),
            SOURCE_FIRST..=SOURCE_LAST => self.modes[offset / 4] as u32,
            SETIP => self.pending,
            CLRIP => self.rectified,
            SETIE => self.enabled,
            GENMSI => self.generated, // Busy is always zero for synchronous software forwarding.
            TARGET_FIRST..=TARGET_LAST => self.targets[(offset - GENMSI) / 4],
            _ => 0, // Reserved/unimplemented S-domain registers are read-only zero.
        }
    }
    pub fn write(&mut self, offset: usize, value: u32, files: &mut [File]) {
        match offset {
            0 => self.delivery = value & 0x100 != 0,
            SOURCE_FIRST..=SOURCE_LAST => {
                let source = offset / 4;
                let bit = 1 << source;
                self.modes[source] = if matches!(value, 0 | 1 | 4..=7) {
                    value as u8
                } else {
                    0
                };
                self.pending &= !bit;
                self.rectified &= !bit;
                if self.modes[source] == 0 {
                    self.enabled &= !bit;
                }
                self.rectify(source);
            }
            SETIP => self.set_pending(value),
            SETIPNUM | SETIPNUM_LE => {
                self.set_pending(Self::number(value));
                self.rearms += 1;
            }
            SETIPNUM_BE => self.set_pending(Self::number(value.swap_bytes())),
            CLRIP => self.pending &= !value,
            CLRIPNUM => self.pending &= !Self::number(value),
            SETIE => self.enabled |= value & self.active(),
            SETIENUM => self.enabled |= Self::number(value) & self.active(),
            CLRIE => self.enabled &= !value,
            CLRIENUM => self.enabled &= !Self::number(value),
            TARGET_FIRST..=TARGET_LAST => {
                self.targets[(offset - GENMSI) / 4] = Self::target(value, files.len())
            }
            GENMSI => {
                self.generated = Self::target(value, files.len());
                Self::send(self.generated, files);
            }
            _ => {}
        }
        self.forward(files);
    }
    fn send(target: u32, files: &mut [File]) -> bool {
        let id = target as usize & 0x7ff;
        let hart = (target >> 18) as usize;
        if id == 0 {
            return false;
        }
        files[hart].inject(id); // All targets were constrained at the MMIO boundary.
        true
    }
    pub fn forward(&mut self, files: &mut [File]) {
        if !self.delivery {
            return;
        }
        let mut remaining = self.pending & self.enabled;
        while remaining != 0 {
            let source = remaining.trailing_zeros() as usize;
            let bit = 1 << source;
            remaining &= !bit;
            if Self::send(self.targets[source], files) {
                self.pending &= !bit;
                self.msis += 1;
            }
        }
    }
    pub fn sources(&self, cpu: usize, id: usize) -> u32 {
        if id == 0 {
            return 0;
        }
        (1..32).fold(0, |mask, source| {
            mask | (u32::from(self.targets[source] == ((cpu as u32) << 18) | id as u32) << source)
        })
    }
    pub fn source_eligible(&self, cpu: usize, source: usize, files: &[File]) -> bool {
        let target = self.targets[source];
        (target >> 18) as usize == cpu && files[cpu].eligible(target as usize & 0x7ff)
    }
    /// Joined device retirement: disconnect this route but never clear an IMSIC
    /// identity that could be shared with another source/MSI producer.
    pub fn retire_source(&mut self, source: u32) {
        let bit = 1 << source;
        self.modes[source as usize] = 0;
        self.targets[source as usize] = 0;
        self.levels &= !bit;
        self.rectified &= !bit;
        self.pending &= !bit;
        self.enabled &= !bit;
    }
}
