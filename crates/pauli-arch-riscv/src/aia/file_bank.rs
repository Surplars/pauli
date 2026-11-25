//! VM-local IMSIC ownership/inboxes, independent of slow device work. If shared,
//! this short lock is below the device lock; owners take ONLY this lock.
//! No CSR, DMA, device polling or logging is allowed while it is held.
//! Ownership/Lock Ordering: VM devices then this short bank lock; owner transfers take only the bank.
//! Invariants: bound files have one physical owner and a separate late-injection inbox.
use super::imsic::{File, WORDS};
use pauli_config::ImsicIds;
use pauli_hal::sync::SpinLock as Mutex;
const CPUS: usize = crate::MAX_VCPUS;

/// A hart publishes its physical file capacity only after clearing the file.
/// An unpublished or borrowed file cannot satisfy a guest request.
#[cfg(any(test, all(target_arch = "riscv64", feature = "aia-guest-file")))]
pub(crate) struct OwnerFile(core::sync::atomic::AtomicUsize);
#[cfg(any(test, all(target_arch = "riscv64", feature = "aia-guest-file")))]
impl OwnerFile {
    pub const fn new() -> Self {
        Self(core::sync::atomic::AtomicUsize::new(0))
    }
    pub fn publish(&self, ids: Option<ImsicIds>) {
        self.0.store(
            ids.map_or(0, ImsicIds::max_id),
            core::sync::atomic::Ordering::Release,
        );
    }
    pub fn acquire(&self, ids: ImsicIds) -> bool {
        use core::sync::atomic::Ordering;
        self.0
            .compare_exchange(ids.max_id(), 0, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HardwareStats {
    #[cfg(all(feature = "selftest", feature = "aia-guest-file"))]
    pub loaded_vcpus: usize,
    pub loads: u64,
    pub puts: u64,
    pub merges: u64,
    pub injected: u64,
    pub bound: usize,
}
pub struct Bank {
    pub(super) files: [File; CPUS],
    cpus: usize,
    pub(super) hardware: HardwareStats,
}
impl Bank {
    pub const fn new(cpus: usize) -> Self {
        Self::with_ids(cpus, ImsicIds::Ids255)
    }
    pub const fn with_ids(cpus: usize, ids: ImsicIds) -> Self {
        assert!(cpus > 0 && cpus <= CPUS);
        Self {
            files: [File::with_ids(ids); CPUS],
            cpus,
            hardware: HardwareStats {
                #[cfg(all(feature = "selftest", feature = "aia-guest-file"))]
                loaded_vcpus: 0,
                loads: 0,
                puts: 0,
                merges: 0,
                injected: 0,
                bound: 0,
            },
        }
    }
    pub const fn ids(&self) -> ImsicIds {
        self.files[0].ids()
    }
    pub fn cpus(&self) -> usize {
        self.cpus
    }
    pub fn line(&self, cpu: usize) -> bool {
        if self.hardware.bound & (1 << cpu) != 0 {
            self.files[cpu].has_pending()
        } else {
            self.files[cpu].line()
        }
    }
    pub fn levels(&self) -> usize {
        (0..self.cpus).fold(0, |mask, cpu| mask | (usize::from(self.line(cpu)) << cpu))
    }
    pub fn take_file(&mut self, cpu: usize) -> File {
        assert!(cpu < self.cpus && self.hardware.bound & (1 << cpu) == 0);
        self.hardware.bound |= 1 << cpu;
        // Unlike bound, this evidence survives put; a new VM epoch gets a fresh bank.
        #[cfg(all(feature = "selftest", feature = "aia-guest-file"))]
        {
            self.hardware.loaded_vcpus |= 1 << cpu;
        }
        self.hardware.loads += 1;
        let empty = File::with_ids(self.ids());
        core::mem::replace(&mut self.files[cpu], empty)
    }
    pub fn take_pending(&mut self, cpu: usize) -> [u64; WORDS] {
        assert!(self.hardware.bound & (1 << cpu) != 0);
        let pending = self.files[cpu].take_pending();
        self.hardware.merges += 1;
        // IMAC has no hardware popcount. Sparse/empty words need no counting.
        self.hardware.injected += pending
            .iter()
            .filter(|bits| **bits != 0)
            .map(|bits| bits.count_ones() as u64)
            .sum::<u64>();
        pending
    }
    /// Merge injections arriving after physical save/clear and before this
    /// lock was acquired; never overwrite the inbox with the older snapshot.
    pub fn put_file(&mut self, cpu: usize, mut file: File) {
        assert!(self.hardware.bound & (1 << cpu) != 0);
        assert_eq!(file.ids(), self.ids());
        file.merge_pending(self.files[cpu].take_pending());
        self.files[cpu] = file;
        self.hardware.bound &= !(1 << cpu);
        self.hardware.puts += 1;
    }
    pub fn hardware(&self) -> HardwareStats {
        self.hardware
    }
    pub fn quiesce(&mut self) {
        assert_eq!(
            self.hardware.bound, 0,
            "join hardware file owners before quiesce"
        );
        let empty = File::with_ids(self.ids());
        self.files.fill(empty);
    }
}
/// Serial/software instances keep inline state. Dedicated owners share a bank
/// lent by the board for the VM epoch; neither path needs heap or raw pointers.
pub(super) enum Storage {
    Inline(Bank),
    Shared(&'static Mutex<Bank>),
}
impl Storage {
    pub fn with<R>(&self, f: impl FnOnce(&Bank) -> R) -> R {
        match self {
            Self::Inline(b) => f(b),
            Self::Shared(b) => f(&b.lock()),
        }
    }
    pub fn with_mut<R>(&mut self, f: impl FnOnce(&mut Bank) -> R) -> R {
        match self {
            Self::Inline(b) => f(b),
            Self::Shared(b) => f(&mut b.lock()),
        }
    }
}
