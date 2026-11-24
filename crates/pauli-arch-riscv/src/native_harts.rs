//! Firmware hart IDs and disjoint worker stacks for one discovered HS topology.
//! Ownership/Lock Ordering: boot computes this immutable map before HSM publication.
//! Invariants: the boot hart keeps its original stack; only dense owners index stacks.
use crate::MAX_CPUS;
use pauli_core::{HvError, Result};

const STACK: usize = 32 * 1024;
pub fn worker_stack_bytes(count: usize) -> Result<usize> {
    if !(1..=MAX_CPUS).contains(&count) {
        return Err(HvError::InvalidConfig);
    }
    Ok((count - 1) * STACK)
}

pub(crate) struct NativeHarts {
    ids: [usize; MAX_CPUS],
    count: usize,
    boot: usize,
    stack_base: usize,
}
impl NativeHarts {
    pub(crate) fn new(ids: &[usize], boot: usize, stacks: &'static mut [u8]) -> Result<Self> {
        let bytes = worker_stack_bytes(ids.len())?;
        let base = stacks.as_mut_ptr() as usize;
        if boot >= ids.len() || stacks.len() != bytes || (bytes != 0 && base & 15 != 0) {
            return Err(HvError::InvalidConfig);
        }
        let mut harts = [0; MAX_CPUS];
        for (i, &hart) in ids.iter().enumerate() {
            if harts[..i].contains(&hart) {
                return Err(HvError::InvalidConfig);
            }
            harts[i] = hart;
        }
        Ok(Self {
            ids: harts,
            count: ids.len(),
            boot,
            stack_base: base,
        })
    }
    pub(crate) fn hart(&self, owner: usize) -> Option<usize> {
        self.ids[..self.count].get(owner).copied()
    }
    pub(crate) fn stack_top(&self, owner: usize) -> Option<usize> {
        if owner >= self.count || owner == self.boot {
            return None;
        }
        let slot = owner - usize::from(owner > self.boot);
        Some(self.stack_base + (slot + 1) * STACK)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{boxed::Box, vec};

    #[test]
    fn sparse_firmware_ids_and_nonzero_boot_share_no_worker_stack() {
        let ids: [usize; MAX_CPUS] = core::array::from_fn(|i| 0x100 + 7 * i);
        for boot in [0, 7, MAX_CPUS - 1] {
            let bytes = worker_stack_bytes(MAX_CPUS).unwrap();
            let storage = Box::leak(vec![0; bytes + 15].into_boxed_slice());
            let pad = (storage.as_ptr() as usize).wrapping_neg() & 15;
            let base = storage.as_ptr() as usize + pad;
            let m = NativeHarts::new(&ids, boot, &mut storage[pad..pad + bytes]).unwrap();
            let mut next = base;
            for (owner, &id) in ids.iter().enumerate() {
                assert_eq!(m.hart(owner), Some(id));
                if owner == boot {
                    assert_eq!(m.stack_top(owner), None);
                    continue;
                }
                next += STACK;
                assert_eq!(m.stack_top(owner), Some(next));
            }
            assert_eq!(next, base + bytes);
            assert_eq!(m.hart(MAX_CPUS), None);
            assert_eq!(m.stack_top(MAX_CPUS), None);
        }
        assert_eq!(worker_stack_bytes(1), Ok(0));
        assert!(worker_stack_bytes(MAX_CPUS + 1).is_err());
        assert!(NativeHarts::new(&[9, 10], 1, &mut []).is_err());
        assert!(NativeHarts::new(&[9], 1, &mut []).is_err());
        assert_eq!(
            NativeHarts::new(&[99], 0, &mut []).unwrap().stack_top(0),
            None
        );
    }
}
