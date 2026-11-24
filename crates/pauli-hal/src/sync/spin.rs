//! Plain short lock for data never accessed by a local native IRQ handler.
//! Ownership/Lock Ordering: caller's device/owner lock -> shared bank lock.
//! Invariants: no IRQ reentry, logging, blocking I/O or guest entry while held.

use core::{
    cell::UnsafeCell,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, Ordering},
};
pub struct SpinLock<T> {
    held: AtomicBool,
    value: UnsafeCell<T>,
}
// SAFETY: atomic exclusion protects the Send payload; no reference escapes a guard.
unsafe impl<T: Send> Sync for SpinLock<T> {}
impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            held: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }
    pub fn lock(&self) -> Guard<'_, T> {
        while self
            .held
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.held.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        Guard {
            lock: self,
            payload: PhantomData,
        }
    }
}
pub struct Guard<'a, T> {
    lock: &'a SpinLock<T>,
    payload: PhantomData<&'a mut T>, // Sharing a guard requires T: Sync, not just T: Send.
}
impl<T> Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: this live guard exclusively owns the value until Release.
        unsafe { &*self.lock.value.get() }
    }
}
impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: unique guard plus mutable borrow excludes every other accessor.
        unsafe { &mut *self.lock.value.get() }
    }
}
impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
    }
}
