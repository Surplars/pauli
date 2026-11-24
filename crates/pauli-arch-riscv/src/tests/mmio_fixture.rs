//! Production algorithm acceptance, using private host fixtures.
//! Ownership/Lock Ordering: each test owns its state; no actual CSR access.
//! Invariants: retain the original protocol assertions and rejection cases.
use crate::{VCpuContext, VmExit, mmio, vstage};
use pauli_hal::addr::{MemPerms, PAGE_SIZE, PhysAddr};
use std::{
    alloc::{Layout, alloc_zeroed, dealloc},
    cell::Cell,
};
use std::{vec, vec::Vec};
pub(super) struct Page {
    pub(super) gpa: usize,
    pub(super) ptr: *mut u8,
    pub(super) executable: bool,
}
impl Page {
    fn new(gpa: usize, executable: bool) -> Self {
        // SAFETY: private aligned page storage satisfies the checked offset, lifetime and allocator layout.
        let ptr = unsafe { alloc_zeroed(Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap()) };
        assert!(!ptr.is_null());
        Self {
            gpa,
            ptr,
            executable,
        }
    }
    pub(super) fn half(&self, offset: usize, value: u16) {
        assert!(offset + 2 <= PAGE_SIZE);
        // SAFETY: private aligned page storage satisfies the checked offset, lifetime and allocator layout.
        unsafe { self.ptr.add(offset).cast::<u16>().write(value) };
    }
    pub(super) fn pte(&self, index: usize, value: u64) {
        assert!(index < 512);
        // SAFETY: private aligned page storage satisfies the checked offset, lifetime and allocator layout.
        unsafe { self.ptr.cast::<u64>().add(index).write(value) };
    }
}
impl Drop for Page {
    fn drop(&mut self) {
        // SAFETY: private aligned page storage satisfies the checked offset, lifetime and allocator layout.
        unsafe {
            dealloc(
                self.ptr,
                Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap(),
            )
        };
    }
}
pub(super) struct MockSpace {
    pub(super) pages: Vec<Page>,
    pub(super) reads: Cell<usize>,
}
impl vstage::ReadOnlyStage2 for MockSpace {
    fn translate(&self, gpa: PhysAddr) -> Option<PhysAddr> {
        self.pages
            .iter()
            .find(|page| gpa.raw() / PAGE_SIZE == page.gpa / PAGE_SIZE)
            .map(|page| PhysAddr::new(page.ptr as usize + gpa.raw() % PAGE_SIZE))
    }
    fn translate_readable(&self, gpa: PhysAddr) -> Option<PhysAddr> {
        self.reads.set(self.reads.get() + 1);
        self.translate(gpa)
    }
    fn is_executable(&self, gpa: PhysAddr) -> bool {
        self.pages
            .iter()
            .any(|page| page.executable && gpa.raw() / PAGE_SIZE == page.gpa / PAGE_SIZE)
    }
    unsafe fn fetch_bare_u32(&self, pc: usize) -> Option<u32> {
        // SAFETY: private aligned page storage satisfies the checked offset, lifetime and allocator layout.
        let low = unsafe { vstage::fetch_u16(self, 0, pc) }?;
        let high = unsafe { vstage::fetch_u16(self, 0, pc.checked_add(2)?) }?;
        Some(u32::from(low) | u32::from(high) << 16)
    }
}
pub(super) const CODE: usize = 0x4000_0000;
pub(super) const DATA: usize = 0x5000_0000;
pub(super) const IO: usize = 0x0c20_0000;
pub(super) fn fixture() -> MockSpace {
    let pages = vec![
        Page::new(0x8100_0000, false),
        Page::new(0x8100_1000, false),
        Page::new(0x8100_2000, false),
        Page::new(0x8100_3000, false),
        Page::new(0x8200_0000, true),
        Page::new(0x8300_0000, true),
    ];
    pages[0].pte(1, vstage::branch_pte(PhysAddr::new(pages[1].gpa)).unwrap());
    pages[1].pte(0, vstage::branch_pte(PhysAddr::new(pages[2].gpa)).unwrap());
    pages[1].pte(
        128,
        vstage::branch_pte(PhysAddr::new(pages[3].gpa)).unwrap(),
    );
    for i in 0..2 {
        pages[2].pte(
            i,
            vstage::leaf_pte(
                PhysAddr::new(pages[4 + i].gpa),
                MemPerms::READ | MemPerms::EXECUTE,
            )
            .unwrap(),
        );
    }
    pages[3].pte(
        0,
        vstage::leaf_pte(PhysAddr::new(IO), MemPerms::READ | MemPerms::WRITE).unwrap(),
    );
    MockSpace {
        pages,
        reads: Cell::new(0),
    }
}
pub(super) fn context(pc: usize) -> VCpuContext {
    let mut x = [0; 32];
    x[11] = DATA;
    x[8] = DATA;
    x[10] = 0x1234_5678;
    VCpuContext {
        pc,
        vsatp: vstage::sv39_satp(PhysAddr::new(0x8100_0000)).unwrap(),
        x,
        exit_hstatus: 1 << 8,
        ..VCpuContext::new()
    }
}
pub(super) fn emulate(
    space: &MockSpace,
    pc: usize,
    store: bool,
    implicit: bool,
) -> (bool, usize, VCpuContext) {
    let mut ctx = context(pc);
    let exit = VmExit {
        cause: if store { 23 } else { 21 },
        from_guest: true,
        pc,
        value: DATA,
        gpa_shifted: IO / 4,
        instruction: if implicit { 0x2000 } else { 0 },
    };
    let mut calls = 0;
    // SAFETY: private aligned page storage satisfies the checked offset, lifetime and allocator layout.
    let okay = unsafe {
        mmio::emulate(&mut ctx, space, exit, &mut |access| {
            calls += 1;
            assert_eq!((access.gpa, access.width), (IO, 4));
            assert_eq!(access.write, if store { Some(0x1234_5678) } else { None });
            Some(0x8765_4321)
        })
    };
    (okay, calls, ctx)
}
