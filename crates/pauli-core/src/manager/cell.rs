//! Pins on removable VM objects backed by their own private host extent.
//! Ownership/Lock Ordering: a pin prevents reclamation, not execution or other readers.
//! Invariants: Empty -> Writer -> Live(+readers) -> Writer -> Empty; only Live has readers.
use crate::{HvError, Result};
use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    ops::Deref,
    sync::atomic::{AtomicUsize, Ordering},
};

const EMPTY: usize = 0;
const WRITER: usize = 1;
const LIVE: usize = 2; // Each transient reader adds one.

pub(crate) struct Cell<T> {
    state: AtomicUsize,
    value: UnsafeCell<*mut T>,
}
// SAFETY: only the CAS writer mutates the value; a shared borrow pins live storage.
unsafe impl<T: Send + Sync> Sync for Cell<T> {}

pub(crate) enum View<'a, T> {
    Fixed(&'a T),
    Managed(Pin<'a, T>),
}
pub(crate) struct Pin<'a, T>(&'a Cell<T>);
pub(super) struct Reservation<'a, T>(&'a Cell<T>);

impl<T> Cell<T> {
    pub(super) const fn new() -> Self {
        Self {
            state: AtomicUsize::new(EMPTY),
            value: UnsafeCell::new(core::ptr::null_mut()),
        }
    }
    pub(super) fn reserve(&self) -> Option<Reservation<'_, T>> {
        self.state
            .compare_exchange(EMPTY, WRITER, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Reservation(self))
    }
    pub(super) fn read(&self) -> Option<View<'_, T>> {
        self.state
            .try_update(Ordering::Acquire, Ordering::Relaxed, |state| {
                if state >= LIVE {
                    state.checked_add(1)
                } else {
                    None
                }
            })
            .ok()
            .map(|_| View::Managed(Pin(self)))
    }
    /// Never wait for a dispatcher. A busy caller can retry after the final
    /// pin leaves; a rejected retirement publishes exactly the original value.
    pub(super) fn remove<R>(&self, retire: impl FnOnce(&mut T) -> Result<R>) -> Result<R> {
        self.state
            .compare_exchange(LIVE, WRITER, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|s| {
                if s == EMPTY {
                    HvError::NotFound
                } else {
                    HvError::Busy
                }
            })?;
        // SAFETY: LIVE with zero pins was exclusively claimed; new readers now fail.
        let ptr = unsafe { *self.value.get() };
        // SAFETY: no other pins exist; Live always points to an initialized object.
        match retire(unsafe { &mut *ptr }) {
            Ok(receipt) => {
                // SAFETY: the writer still owns the initialized value, with no readers.
                unsafe {
                    ptr.drop_in_place();
                    *self.value.get() = core::ptr::null_mut();
                }
                self.state.store(EMPTY, Ordering::Release);
                Ok(receipt)
            }
            Err(error) => {
                self.state.store(LIVE, Ordering::Release);
                Err(error)
            }
        }
    }
}
impl<T> Reservation<'_, T> {
    pub(super) fn publish(self, storage: &'static mut MaybeUninit<T>, value: T) {
        // SAFETY: the writer exclusively owns the pointer until release-publication.
        unsafe { *self.0.value.get() = storage.write(value) };
        self.0.state.store(LIVE, Ordering::Release);
        core::mem::forget(self); // Publication consumed the reservation.
    }
}
impl<T> Drop for Reservation<'_, T> {
    fn drop(&mut self) {
        self.0.state.store(EMPTY, Ordering::Release);
    }
}
impl<T> Deref for View<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        match self {
            Self::Fixed(value) => value,
            // SAFETY: the pin prevents removal for this borrow's entire lifetime.
            Self::Managed(pin) => unsafe { &**pin.0.value.get() },
        }
    }
}
impl<T> Drop for Pin<'_, T> {
    fn drop(&mut self) {
        self.0.state.fetch_sub(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Barrier, thread};

    #[test]
    fn unpublished_and_pinned_storage_cannot_be_recycled() {
        let cell = Cell::new();
        let abandoned = cell.reserve().unwrap();
        assert!(cell.reserve().is_none() && cell.read().is_none());
        drop(abandoned);
        let backing = || std::boxed::Box::leak(std::boxed::Box::new(MaybeUninit::uninit()));
        cell.reserve().unwrap().publish(backing(), 7usize);
        let sync = Barrier::new(2);
        thread::scope(|scope| {
            scope.spawn(|| {
                let old = cell.read().unwrap();
                sync.wait();
                sync.wait();
                assert_eq!(*old, 7);
            });
            sync.wait();
            assert_eq!(cell.remove(|_| Ok(())), Err(HvError::Busy));
            sync.wait();
        });
        assert_eq!(
            cell.remove(|_| Err::<(), _>(HvError::Permission)),
            Err(HvError::Permission)
        );
        assert_eq!(cell.remove(|old| Ok(*old)), Ok(7));
        assert!(cell.read().is_none());
        cell.reserve().unwrap().publish(backing(), 11);
        assert_eq!(*cell.read().unwrap(), 11);
    }
}
