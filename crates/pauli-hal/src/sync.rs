//! IRQ-safe spinlock and one-time publication; CPU details are supplied by the ISA.
//! Ownership/Lock Ordering: disable local IRQ -> acquire; release -> restore IRQ.
//! Invariants: guards never move CPUs; no lock may be held across guest execution.

use core::{
    cell::UnsafeCell,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, Ordering},
};
mod once;
pub use once::Once;
mod spin;
pub use spin::SpinLock;

/// Local interrupt state, including whether interrupts were already disabled.
/// # Safety
/// `save` must mask local host interrupts before returning. `restore` must
/// restore exactly its saved state, on the same CPU and in nesting order.
pub unsafe trait LocalIrq: Send + Sync + 'static {
    type Saved: Copy;
    fn save() -> Self::Saved;
    /// # Safety
    /// The token must come from this CPU's matching, innermost `save` call.
    unsafe fn restore(saved: Self::Saved);
}

pub struct IrqSpinLock<T, I: LocalIrq> {
    held: AtomicBool,
    value: UnsafeCell<T>,
    irq: PhantomData<I>,
}

// SAFETY: the atomic lock grants one exclusive accessor; I masks local IRQ reentry.
unsafe impl<T: Send, I: LocalIrq> Sync for IrqSpinLock<T, I> {}
// SAFETY: moving an unborrowed lock also moves its Send payload, not a live guard.
unsafe impl<T: Send, I: LocalIrq> Send for IrqSpinLock<T, I> {}

impl<T, I: LocalIrq> IrqSpinLock<T, I> {
    pub const fn new(value: T) -> Self {
        Self {
            held: AtomicBool::new(false),
            value: UnsafeCell::new(value),
            irq: PhantomData,
        }
    }
    pub fn lock(&self) -> Guard<'_, T, I> {
        let saved = I::save();
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
            saved,
            local: PhantomData,
        }
    }
    pub fn try_lock(&self) -> Option<Guard<'_, T, I>> {
        let saved = I::save();
        if self
            .held
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(Guard {
                lock: self,
                saved,
                local: PhantomData,
            })
        } else {
            // SAFETY: no intervening save/guard; undo this CPU's unsuccessful attempt.
            unsafe {
                I::restore(saved);
            }
            None
        }
    }
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }
}

pub struct Guard<'a, T, I: LocalIrq> {
    lock: &'a IrqSpinLock<T, I>,
    saved: I::Saved,
    local: PhantomData<*mut ()>, // !Send/!Sync: IRQ state belongs to this CPU.
}
impl<T, I: LocalIrq> Deref for Guard<'_, T, I> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: this guard holds the acquire/release lock for the borrow's lifetime.
        unsafe { &*self.lock.value.get() }
    }
}
impl<T, I: LocalIrq> DerefMut for Guard<'_, T, I> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the guard and this mutable borrow exclude every other accessor.
        unsafe { &mut *self.lock.value.get() }
    }
}
impl<T, I: LocalIrq> Drop for Guard<'_, T, I> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
        // SAFETY: !Send keeps this guard on its CPU; restore only after unlocking.
        unsafe {
            I::restore(self.saved);
        }
    }
}
