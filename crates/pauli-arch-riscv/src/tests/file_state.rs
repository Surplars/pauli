//! Complete physical-file transfer and late software inbox retention.
//! Ownership/Lock Ordering: one test owner; producer/owner transitions are explicit.
//! Invariants: no lost late injection, no pending or enable state on reuse.
use crate::aia::{
    file_state::{self, Registers},
    imsic::{File, WORDS},
    registers::{EIE, EIP},
    virtualized::VirtualAia,
};
use pauli_config::ImsicIds;
struct RegisterFile {
    ids: ImsicIds,
    words: [usize; 256],
    opens: usize,
    closes: usize,
    writes: usize,
}
impl RegisterFile {
    fn new() -> Self {
        Self::with_ids(ImsicIds::Ids255)
    }
    fn with_ids(ids: ImsicIds) -> Self {
        Self {
            ids,
            words: [0; 256],
            opens: 0,
            closes: 0,
            writes: 0,
        }
    }
    fn check(&self, select: usize) {
        for base in [EIP, EIE] {
            if (base..base + 2 * WORDS).contains(&select) {
                assert_eq!((select - base) % 2, 0);
                assert!((select - base) / 2 <= self.ids.max_id() / 64);
            }
        }
    }
    fn open(&mut self) -> Window<'_> {
        self.opens += 1;
        Window(self)
    }
}
/// Acquiring and dropping the window models selecting and restoring the hardware file.
struct Window<'a>(&'a mut RegisterFile);
impl Drop for Window<'_> {
    fn drop(&mut self) {
        self.0.closes += 1;
    }
}
impl Registers for Window<'_> {
    fn read(&mut self, select: usize) -> usize {
        self.0.check(select);
        self.0.words[select]
    }
    fn write(&mut self, select: usize, value: usize) {
        self.0.check(select);
        self.0.writes += 1;
        self.0.words[select] = value;
    }
}
#[test]
fn owner_files_require_a_successful_local_probe_and_matching_vm_size() {
    use crate::aia::file_bank::OwnerFile;
    use pauli_config::ImsicIds::{Ids127, Ids255};
    let [primary, secondary] = [const { OwnerFile::new() }; 2];
    assert!(!primary.acquire(Ids127));
    primary.publish(Some(Ids127));
    assert!(!secondary.acquire(Ids127));
    secondary.publish(Some(Ids255));
    assert!(!primary.acquire(Ids255));
    assert!(!secondary.acquire(Ids127));
    assert!(primary.acquire(Ids127));
    assert!(!primary.acquire(Ids127));
    assert!(secondary.acquire(Ids255));
    primary.publish(Some(Ids127));
    secondary.publish(Some(Ids255));
    primary.publish(None); // Reprobing this hart failed before dispatch.
    assert!(!primary.acquire(Ids127));
    assert!(secondary.acquire(Ids255));
}

#[test]
fn complete_bank_roundtrips_and_clears() {
    for ids in [ImsicIds::Ids127, ImsicIds::Ids255] {
        for id in 1..=ids.max_id() {
            let mut regs = RegisterFile::with_ids(ids);
            let mut file = File::with_ids(ids);
            file.indirect(EIE + (id / 64) * 2, usize::MAX, 1 << (id % 64), true)
                .unwrap();
            file.indirect(0x70, usize::MAX, 1, true).unwrap();
            file.inject(id);
            file_state::load(&mut regs.open(), file);
            let saved = file_state::save_clear(&mut regs.open(), ids);
            assert_eq!(saved.ids(), ids);
            assert_eq!(saved.top(), id);
            assert!(saved.line());
            assert!(regs.words.iter().all(|word| *word == 0));
            file_state::load(&mut regs.open(), File::with_ids(ids));
            assert_eq!(file_state::save_clear(&mut regs.open(), ids).top(), 0);
        }
    }
}
#[test]
fn narrow_file_retains_physical_threshold_and_ignores_reserved_bits() {
    use crate::aia::registers::{EIDELIVERY, EITHRESHOLD};
    let ids = ImsicIds::Ids127;
    let mut regs = RegisterFile::with_ids(ids);
    regs.words[EITHRESHOLD] = 129;
    regs.words[EIDELIVERY] = 1;
    regs.words[EIP] = 1;
    regs.words[EIE] = 1;
    regs.words[EIP + 2] = 1 << 63;
    regs.words[EIE + 2] = 1 << 63;
    let mut file = file_state::save_clear(&mut regs.open(), ids);
    assert_eq!(file.indirect(EITHRESHOLD, 0, 0, false), Some(129));
    assert_eq!(file.top(), 127);
    assert!(file.line());
    assert_eq!(file.indirect(EIP, 0, 0, false), Some(0));
    assert_eq!(file.indirect(EIE, 0, 0, false), Some(0));
    file_state::load(&mut regs.open(), file);
    assert_eq!(regs.words[EITHRESHOLD], 129);
    assert_eq!(file_state::save_clear(&mut regs.open(), ids), file);
    let opens = regs.opens;
    file_state::merge(ids, [1, 0, !0, !0], || regs.open());
    assert_eq!(regs.opens, opens);
    file_state::merge(ids, [1, 1 << 63, !0, !0], || regs.open());
    assert_eq!(regs.opens, opens + 1);
    assert_eq!(regs.words[EIP + 2], 1 << 63);
    assert_eq!(regs.words[EIP + 4], 0);
    assert_eq!(regs.words[EIP], 0);
}

#[test]
fn merge_opens_one_window_only_for_nonempty_words() {
    // One bit is already pending, one overlaps, and one is new.
    const OLD: usize = 0b0110;
    const INCOMING: u64 = 0b1010;
    for mask in 0usize..(1 << WORDS) {
        let mut regs = RegisterFile::new();
        let pending = core::array::from_fn(|word| {
            regs.words[EIP + 2 * word] = OLD;
            if mask & (1 << word) != 0 { INCOMING } else { 0 }
        });
        file_state::merge(ImsicIds::Ids255, pending, || regs.open());
        let windows = usize::from(mask != 0);
        assert_eq!(
            (regs.opens, regs.closes),
            (windows, windows),
            "mask={mask:#x}"
        );
        assert_eq!(regs.writes, mask.count_ones() as usize, "mask={mask:#x}");
        for (word, bits) in pending.into_iter().enumerate() {
            assert_eq!(regs.words[EIP + 2 * word], OLD | bits as usize);
        }
    }
}
#[test]
fn inbox_merge_survives_hardware_save_and_peer_restart() {
    let mut vm = VirtualAia::new(2);
    let mut peer = VirtualAia::new(1);
    vm.msi_access(0, Some(37));
    let mut file = vm.take_file(0);
    let mut regs = RegisterFile::new();
    file.indirect(EIE, 0, 1 << 37, true).unwrap();
    file_state::load(&mut regs.open(), file);
    vm.msi_access(0, Some(63));
    file_state::merge(vm.ids(), vm.take_pending(0), || regs.open());
    file = file_state::save_clear(&mut regs.open(), vm.ids());
    vm.msi_access(0, Some(255)); // Arrives after physical save, before put.
    peer.msi_access(0, Some(52));
    vm.put_file(0, file);
    let mut saved = vm.take_file(0);
    for word in 0..4 {
        saved
            .indirect(0xc0 + word * 2, usize::MAX, usize::MAX, true)
            .unwrap();
    }
    for id in [37, 63, 255] {
        assert_eq!(saved.topei(true), (id << 16) | id);
    }
    assert_eq!(saved.topei(true), 0);
    vm.put_file(0, saved);
    vm.quiesce();
    let mut peer_file = peer.take_file(0);
    peer_file
        .indirect(0xc0, usize::MAX, usize::MAX, true)
        .unwrap();
    assert_eq!(peer_file.top(), 52);
    peer.put_file(0, peer_file);
    assert_eq!(vm.hardware().bound, 0);
    assert_eq!(vm.hardware().loads, vm.hardware().puts);

    // The replacement reuses the physical window, not the old VM's pending or enable bits.
    let mut next = VirtualAia::new(1);
    file_state::load(&mut regs.open(), next.take_file(0));
    file_state::merge(next.ids(), next.take_pending(0), || regs.open());
    let clean = file_state::save_clear(&mut regs.open(), next.ids());
    assert_eq!(clean, File::new());
    next.put_file(0, clean);
}

#[cfg(feature = "selftest")]
#[test]
fn software_delivery_never_attests_a_native_file() {
    use crate::{irq_backend::RiscvIrqChip, plic::VirtualPlic};
    use pauli_core::arch::{IrqFileStats, IrqRegion, RegisterAccess, VirtualIrqChip};
    assert!(
        RiscvIrqChip::Legacy(VirtualPlic::new())
            .file_stats()
            .is_none()
    );
    let mut chip = RiscvIrqChip::Advanced(VirtualAia::new(2));
    let write = |register, select, set| RegisterAccess {
        register,
        select,
        clear: usize::MAX,
        set,
        write: true,
    };
    // The software fallback can actually deliver and claim the MSI on vCPU 1.
    assert_eq!(chip.register(1, write(0x151, 0xc0, 1 << 37)), Some(0));
    assert_eq!(chip.register(1, write(0x151, 0x70, 1)), Some(0));
    assert_eq!(chip.access(IrqRegion::Message, 4096, Some(37)), Some(0));
    assert!(chip.pending(1));
    assert_eq!(chip.register(1, write(0x15c, 0, 0)), Some(37 << 16 | 37));
    assert!(!chip.pending(1));
    assert_eq!(chip.file_stats(), Some(IrqFileStats::default()));
}

#[cfg(all(feature = "selftest", feature = "aia-guest-file"))]
#[test]
fn native_transfer_evidence_belongs_to_the_chip_epoch() {
    use crate::irq_backend::RiscvIrqChip;
    use pauli_core::arch::{IrqFileStats, VirtualIrqChip};
    let mut vm = VirtualAia::new(2);
    for cpu in [1, 0] {
        let mut file = vm.take_file(cpu);
        assert_eq!(vm.hardware().bound, 1 << cpu);
        vm.msi_access(4096 * cpu, Some(37));
        file.merge_pending(vm.take_pending(cpu));
        vm.put_file(cpu, file);
    }
    vm.quiesce(); // Joined, but still the same epoch; its terminal evidence is retained.
    assert_eq!(
        RiscvIrqChip::Advanced(vm).file_stats(),
        Some(IrqFileStats {
            loaded_vcpus: 3,
            loads: 2,
            puts: 2,
            merges: 2,
            injected: 2,
            bound: 0
        })
    );
    let replacement = RiscvIrqChip::Advanced(VirtualAia::new(2));
    assert_eq!(replacement.file_stats(), Some(IrqFileStats::default()));
}
