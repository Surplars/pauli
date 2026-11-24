//! Single-assignment publication for boot-installed services.
//! Ownership/Lock Ordering: CAS owns initialization; readers only acquire Ready.
//! Invariants: Empty -> Initializing -> Ready; stored references cannot be replaced.

use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicU8, Ordering},
};
const EMPTY: u8 = 0;
const INITIALIZING: u8 = 1;
const READY: u8 = 2;

pub struct Once<T> {
    state: AtomicU8,
    value: UnsafeCell<MaybeUninit<T>>,
}
// SAFETY: only the successful CAS writer mutates; Ready release-publishes Sync data.
unsafe impl<T: Send + Sync> Sync for Once<T> {}

impl<T> Once<T> {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(EMPTY),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
    pub fn set(&self, value: T) -> Result<(), T> {
        if self
            .state
            .compare_exchange(EMPTY, INITIALIZING, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(value);
        }
        // SAFETY: this writer exclusively owns uninitialized storage until Ready.
        unsafe {
            (*self.value.get()).write(value);
        }
        self.state.store(READY, Ordering::Release);
        Ok(())
    }
    pub fn get(&self) -> Option<&T> {
        if self.state.load(Ordering::Acquire) != READY {
            return None;
        }
        // SAFETY: Ready proves initialization and no writer can replace the value.
        Some(unsafe { (*self.value.get()).assume_init_ref() })
    }
}
impl<T> Default for Once<T> {
    fn default() -> Self {
        Self::new()
    }
}
impl<T> Drop for Once<T> {
    fn drop(&mut self) {
        if *self.state.get_mut() == READY {
            // SAFETY: exclusive ownership of Once; its value was initialized exactly once.
            unsafe {
                self.value.get_mut().assume_init_drop();
            }
        }
    }
}
