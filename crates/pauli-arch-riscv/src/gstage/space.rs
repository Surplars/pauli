//! Construct the private Sv39x4 address space before any vCPU or device is live.
//! Ownership/Lock Ordering: the builder exclusively owns the arena; no locks or CSR writes.
//! Invariants: ordinary firmware leaves enforce W^X; only private Linux RAM uses superpages.
use super::GStagePageTable;
use pauli_core::{HvError, Result, arch::Stage2Space, board::Payload};
use pauli_hal::addr::{MemPerms, PAGE_SIZE, PhysAddr};
// SAFETY: only the returned G-stage owner retains arena pointers. The builder
// does not install a translation or publish pointers to any external registry.
unsafe impl Stage2Space for GStagePageTable {
    unsafe fn create(
        ram: &pauli_core::memory::GuestRam,
        arena: &mut pauli_core::memory::Arena,
        payload: Payload,
    ) -> Result<Self> {
        use crate::gstage::{GStageRoot, GStageTable, ROOT_SIZE};
        const BLOCK: usize = pauli_config::ALIGN as usize;
        let host = ram.host_range().base();
        let guest = ram.guest_base();
        let end = guest.checked_add(ram.len()).ok_or(HvError::InvalidMemory)?;
        let pages = match payload {
            Payload::Linux => None,
            Payload::Embedded => Some((1, 1, ram.len() / PAGE_SIZE)),
            Payload::Firmware(layout) => {
                let rx = usize::from(layout.rx_pages);
                let rom = rx + usize::from(layout.ro_pages);
                let dtb = ram
                    .len()
                    .checked_sub(pauli_config::MAX_GUEST_DTB)
                    .ok_or(HvError::InvalidMemory)?
                    / PAGE_SIZE;
                if rx == 0 || rom >= dtb {
                    return Err(HvError::InvalidMemory);
                }
                Some((rx, rom, dtb))
            }
        };
        let align = if pages.is_none() { BLOCK } else { PAGE_SIZE };
        if host % align != 0 || guest % align != 0 || ram.len() % align != 0 || end > 1 << 41 {
            return Err(HvError::InvalidMemory);
        }
        let roots = ((end - 1) >> 30) - (guest >> 30) + 1;
        let tables = if payload == Payload::Linux {
            roots
        } else {
            roots + ((end - 1) >> 21) - (guest >> 21) + 1
        };
        let root = arena.take(ROOT_SIZE, ROOT_SIZE)?;
        let lower = arena.take(tables * core::mem::size_of::<GStageTable>(), 4096)?;
        // SAFETY: arena lends unique aligned private storage; all bit patterns in
        // these u64 table structures are valid. new_identity initializes every entry.
        let mut space = unsafe {
            GStagePageTable::new_identity(
                &mut *root.as_mut_ptr().cast::<GStageRoot>(),
                core::slice::from_raw_parts_mut(lower.as_mut_ptr().cast::<GStageTable>(), tables),
            )
        };
        for offset in (0..ram.len()).step_by(align) {
            let gpa = PhysAddr::new(guest + offset);
            let hpa = PhysAddr::new(host + offset);
            let mapped = if let Some((rx, rom, dtb)) = pages {
                let page = offset / PAGE_SIZE;
                let permissions = if page < rx {
                    MemPerms::READ | MemPerms::EXECUTE
                } else if page < rom || page >= dtb {
                    MemPerms::READ
                } else {
                    MemPerms::READ | MemPerms::WRITE
                };
                space.map_page(gpa, hpa, permissions)
            } else {
                // SAFETY: caller assigned this retained private RAM; no live table edit.
                unsafe { space.map_private_ram_2m(gpa, hpa) }
            };
            mapped.map_err(|_| HvError::InvalidMemory)?;
        }
        Ok(space)
    }
    fn executable(&self, address: usize) -> bool {
        self.is_executable(PhysAddr::new(address))
    }
}

#[cfg(test)]
#[test]
fn real_firmware_ptes_confine_code_data_and_dtb_to_their_own_pages() {
    use super::{R, W, X};
    use pauli_core::memory::{Arena, GuestRam};
    use pauli_hal::mem::PhysRange;
    use std::boxed::Box;
    #[repr(align(4096))]
    struct Ram([u8; 9 * PAGE_SIZE]);
    let mut data = Ram([0; 9 * PAGE_SIZE]);
    let base = data.0.as_mut_ptr() as usize;
    let gpa = 0x4000;
    let backing = PhysRange::new(base, data.0.len()).unwrap();
    // SAFETY: the fixture transfers this exclusive allocation through the last PTE lookup.
    let ram = unsafe { GuestRam::new(backing, gpa) }.unwrap();
    let mut arena = Arena::new(Box::leak(std::vec![0; 64 * 1024].into_boxed_slice()));
    let payload = Payload::Firmware(pauli_config::FirmwareLayout {
        rx_pages: 2,
        ro_pages: 1,
    });
    // SAFETY: private, unpublished RAM/tables, with no hardware or device observer.
    let space = unsafe { GStagePageTable::create(&ram, &mut arena, payload) }.unwrap();
    for (n, expected) in [R | X, R | X, R, R | W, R | W, R, R, R, R]
        .into_iter()
        .enumerate()
    {
        let address = PhysAddr::new(gpa + n * PAGE_SIZE);
        assert_eq!(space.leaf(address).unwrap().0 & (R | W | X), expected);
        assert_eq!(
            space.translate(address).unwrap().raw(),
            base + n * PAGE_SIZE
        );
    }
    assert!(space.translate(PhysAddr::new(gpa - 1)).is_none());
    assert!(space.translate(PhysAddr::new(gpa + ram.len())).is_none());
}
