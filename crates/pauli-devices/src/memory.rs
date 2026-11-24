//! Checked guest RAM access shared by software devices. No MMIO, host memory,
//! firmware or other VM may pass check; permissions apply to the entire range.
//! Ownership/Lock Ordering: caller keeps RAM assigned throughout each copy; no locks.
//! Invariants: implementations validate the entire range before touching bytes.
pub trait GuestMemory {
    fn check(&self, gpa: u64, len: usize, write: bool) -> bool;
    fn read(&self, gpa: u64, out: &mut [u8]) -> bool;
    fn write(&mut self, gpa: u64, data: &[u8]) -> bool;

    /// Payload-only accesses let diagnostic wrappers separate bulk data from
    /// descriptor/header/status traffic. The same checked RAM policy applies;
    /// there is no borrowed guest slice or DMA permission implied by this API.
    fn read_payload(&self, gpa: u64, out: &mut [u8]) -> bool {
        self.read(gpa, out)
    }
    fn write_payload(&mut self, gpa: u64, data: &[u8]) -> bool {
        self.write(gpa, data)
    }
}
