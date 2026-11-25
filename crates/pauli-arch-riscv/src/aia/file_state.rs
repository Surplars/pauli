//! Complete transfer of the physical file's implemented identities. The caller owns a stopped guest file with
//! no physical MSI producers; software producers use the VM's separate inbox.
//! This is shared by the real CSR window and the offline register model.
//! Ownership/Lock Ordering: exclusive stopped physical file; producers use the software inbox.
//! Invariants: save clears all IDs; late inbox injections merge rather than overwrite.
use super::imsic::{File, WORDS, word_count};
use super::registers::{EIDELIVERY, EIE, EIP, EITHRESHOLD};
use pauli_config::ImsicIds;

pub trait Registers {
    fn read(&mut self, select: usize) -> usize;
    fn write(&mut self, select: usize, value: usize);
    fn take(&mut self, select: usize) -> usize {
        let old = self.read(select);
        self.write(select, 0);
        old
    }
    fn set(&mut self, select: usize, bits: usize) {
        let old = self.read(select);
        self.write(select, old | bits);
    }
}
/// The pool clears every file on init/put. Load only nonzero state into that
/// known-empty file; no speculative CSR readback or repeated zero writes.
pub fn load(regs: &mut impl Registers, file: File) {
    for word in 0..word_count(file.ids()) {
        if file.pending[word] != 0 {
            regs.write(EIP + word * 2, file.pending[word] as usize);
        }
        if file.enabled[word] != 0 {
            regs.write(EIE + word * 2, file.enabled[word] as usize);
        }
    }
    if file.threshold != 0 {
        regs.write(EITHRESHOLD, file.threshold);
    }
    if file.delivery {
        regs.write(EIDELIVERY, 1);
    }
}
/// Open the resident file only for bits taken from this vCPU's inbox.
/// A later injection stays in the inbox for the next merge or put.
pub fn merge<R: Registers>(ids: ImsicIds, mut pending: [u64; WORDS], open: impl FnOnce() -> R) {
    pending[0] &= !1;
    let implemented = &pending[..word_count(ids)];
    // Avoid opening a hardware window for an empty or out-of-range inbox.
    if implemented.iter().all(|&bits| bits == 0) {
        return;
    }
    let mut regs = open();
    for (word, &bits) in implemented.iter().enumerate() {
        if bits != 0 {
            let select = EIP + word * 2;
            regs.set(select, bits as usize);
        }
    }
}
pub fn save_clear(regs: &mut impl Registers, ids: ImsicIds) -> File {
    let mut file = File::with_ids(ids);
    file.delivery = regs.take(EIDELIVERY) == 1;
    // A physical file can implement a wider threshold than its largest identity.
    // Preserve the register: narrowing it could mask an eligible interrupt.
    file.threshold = regs.take(EITHRESHOLD);
    for word in 0..word_count(ids) {
        file.pending[word] = regs.take(EIP + word * 2) as u64;
        file.enabled[word] = regs.take(EIE + word * 2) as u64;
    }
    file.pending[0] &= !1;
    file.enabled[0] &= !1;
    file
}
