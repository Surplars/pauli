//! Real bus, foreign copies and atomic lifecycle tests without native execution.
//! Ownership/Lock Ordering: private fixtures; concurrent tests use production atomics.
//! Invariants: denied copies never modify bytes; stop freezes admission before join.
use crate::{
    bus::{DeviceId, MmioBus},
    memory::{Arena, GuestRam},
    vm::lifecycle::{Lifecycle, State},
};
use pauli_config::MmioRegion;
use pauli_devices::memory::GuestMemory;
use pauli_hal::{device::MmioAccess, mem::PhysRange};
use std::{
    boxed::Box,
    sync::{Arc, Barrier},
    thread, vec,
};
#[test]
fn bus_permissions_and_boundaries() {
    let mut bus = MmioBus::<2>::new();
    let region = MmioRegion {
        base: 0x2000,
        size: 32,
    };
    bus.register(region, DeviceId::Uart).unwrap();
    assert!(bus.register(region, DeviceId::Rtc).is_err());
    assert!(
        bus.register(
            MmioRegion {
                base: u64::MAX,
                size: 8
            },
            DeviceId::Net
        )
        .is_err()
    );
    let access = MmioAccess {
        gpa: 0x2018,
        width: 8,
        write: None,
    };
    assert_eq!(bus.resolve(access), Some((DeviceId::Uart, 24)));
    for access in [
        MmioAccess {
            gpa: 0x201c,
            ..access
        },
        MmioAccess {
            gpa: 0x2020,
            ..access
        },
        MmioAccess { width: 3, ..access },
    ] {
        assert_eq!(bus.resolve(access), None);
    }
    assert!(bus.validate_ram(0x2010, 64).is_err());
    assert!(bus.validate_ram(0x2020, 64).is_ok());
}
#[test]
fn exact_range_foreign_copies_and_private_arena() {
    let mut bytes = [0u64; 16];
    let host = bytes.as_mut_ptr() as usize;
    let range = PhysRange::new(host, core::mem::size_of_val(&bytes)).unwrap();
    // SAFETY: private fixture storage has no outstanding Rust aliases during this scope.
    let mut ram = unsafe { GuestRam::new(range, 0x5000) }.unwrap();
    for offset in 0..8 {
        let input = [0x31u8; 37];
        assert!(ram.write(0x5000 + offset, &input));
        let mut output = [0; 37];
        assert!(ram.read(0x5000 + offset, &mut output));
        assert_eq!(input, output);
    }
    let mut untouched = [0xa5; 8];
    assert!(!ram.read(0x4fff, &mut untouched));
    assert!(!ram.write(u64::MAX, &[1, 2]));
    assert_eq!(untouched, [0xa5; 8]);
    assert!(ram.check(0x5080, 0, false));
    assert!(!ram.check(0x5080, 1, false));
    drop(ram);
    let mut arena = Arena::new(Box::leak(vec![0u8; 128].into_boxed_slice()));
    let a = arena.take(24, 16).unwrap();
    let b = arena.take(24, 16).unwrap();
    assert_eq!(a.as_ptr() as usize % 16, 0);
    assert!(a.as_ptr_range().end <= b.as_ptr_range().start);
    assert!(arena.take(128, 8).is_err());
}
#[test]
fn typed_arena_drops_exactly_the_initialized_prefix() {
    use crate::{HvError, memory::ArenaSlice};
    use std::{cell::Cell, rc::Rc};
    #[repr(align(64))]
    struct Spy(Rc<Cell<usize>>);
    impl Drop for Spy {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    let drops = Rc::new(Cell::new(0));
    let budget = ArenaSlice::<Spy>::budget(16).unwrap();
    let bytes = Box::leak(vec![0; 1 + 2 * budget].into_boxed_slice());
    let mut arena = Arena::new(&mut bytes[1..]);
    // The half-built array never became a VM, but its owned resources still retire.
    let failed = ArenaSlice::build(&mut arena, 16, |i| {
        if i == 9 {
            Err(HvError::Fault)
        } else {
            Ok(Spy(drops.clone()))
        }
    });
    assert!(matches!(failed, Err(HvError::Fault)));
    assert_eq!(drops.get(), 9);
    let live = ArenaSlice::build(&mut arena, 16, |_| Ok(Spy(drops.clone()))).unwrap();
    assert_eq!((live.len(), live.as_ptr() as usize % 64), (16, 0));
    drop(live);
    assert_eq!(drops.get(), 25);
}

#[test]
fn lifecycle_stop_and_fault_ordering() {
    let mut life = Lifecycle::new();
    assert!(!life.acquire_owner(0));
    life.publish();
    assert!(life.acquire_owner(0));
    life.request_stop(false, true);
    assert!(!life.can_enter());
    assert!(!life.commit_join(true, true));
    life.release_owner(0);
    assert!(!life.commit_join(false, true));
    assert!(!life.commit_join(true, false));
    life.cancel_reset();
    life.request_stop(false, true); // A late guest reset cannot override host stop.
    assert!(!life.reset_requested());
    assert!(life.commit_join(true, true));
    assert_eq!(life.state(), State::Stopped);
    life.begin_rebuild().unwrap().publish().unwrap();
    life.request_stop(true, true);
    assert!(life.commit_join(true, true));
    assert_eq!(life.state(), State::Faulted);
    assert!(!life.reset_requested());
}
#[test]
fn joined_freeze_races_owner_acquisition() {
    for _ in 0..128 {
        let mut life = Lifecycle::new();
        life.publish();
        life.request_stop(false, false);
        let life = Arc::new(life);
        let barrier = Arc::new(Barrier::new(2));
        let other = life.clone();
        let rendezvous = barrier.clone();
        let owner = thread::spawn(move || {
            rendezvous.wait();
            if other.acquire_owner(0) {
                assert!(!other.joined());
                other.release_owner(0);
            }
        });
        barrier.wait();
        life.commit_join(true, true);
        owner.join().unwrap();
        assert!(life.commit_join(true, true));
        assert!(!life.acquire_owner(0));
    }
}
#[test]
fn rebuild_excludes_parallel_start_and_obeys_new_stop() {
    let life = Lifecycle::new();
    let rebuild = life.begin_rebuild().unwrap();
    assert!(life.begin_rebuild().is_err());
    assert!(!life.acquire_owner(0));
    life.cancel_reset();
    assert!(rebuild.publish().is_err());
    assert!(life.joined());
    assert!(!life.can_enter());
    life.begin_rebuild().unwrap().publish().unwrap();
    assert!(life.can_enter());
}
