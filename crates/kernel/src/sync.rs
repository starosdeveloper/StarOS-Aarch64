//! The lock the single-core era did without.
//!
//! Every shared structure in this kernel — the scheduler, the frame allocator,
//! the object table, the heap — has until now been an `UnsafeCell` static whose
//! safety comment said, in one wording or another, "single-core, so nothing else
//! can be looking". Starting a second core deletes that sentence everywhere it
//! appears at once. This module is what replaces it.
//!
//! # Why a ticket lock
//!
//! The obvious spinlock is a `compare_exchange` on a flag, and it is unfair: on
//! contention the winner is whoever's cache line happens to be closest, so a core
//! can be starved indefinitely while others hand the lock back and forth. A
//! kernel that schedules is exactly where that shows up as a task that never
//! runs. A ticket lock costs one extra word and hands the lock over in request
//! order, which turns "unlucky" into "waits its turn".
//!
//! # Why locking implies masking interrupts
//!
//! An interrupt handler runs on the core it interrupted. If it takes a lock that
//! core already holds, the core waits for itself forever — a deadlock with one
//! core and no concurrency in sight, which is why this is not an SMP-only
//! concern. Any lock an interrupt handler might touch must therefore be taken
//! with interrupts masked on the taking core, and [`SpinLock::lock`] does that
//! unconditionally rather than leaving it to each caller to remember.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

use staros_arch_aarch64::exceptions;

/// A ticket spinlock that also masks interrupts on the holding core.
pub struct SpinLock<T> {
    /// The next ticket to hand out.
    next: AtomicU32,
    /// The ticket currently being served.
    serving: AtomicU32,
    data: UnsafeCell<T>,
}

// SAFETY: the lock is what makes `&T` from multiple cores sound — access to the
// data is only ever handed out through a guard, and only one guard exists at a
// time. `T: Send` because the value is effectively moved between cores.
unsafe impl<T: Send> Sync for SpinLock<T> {}
// SAFETY: as above; sending the lock itself sends the data.
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Wrap `data` in a lock.
    pub const fn new(data: T) -> Self {
        Self {
            next: AtomicU32::new(0),
            serving: AtomicU32::new(0),
            data: UnsafeCell::new(data),
        }
    }

    /// Take the lock, masking interrupts on this core until the guard is dropped.
    pub fn lock(&self) -> SpinGuard<'_, T> {
        // Mask *first*. Taking the ticket and then being interrupted into
        // something that wants this lock would deadlock this core against itself.
        // SAFETY: the guard restores the previous state on drop, on this core.
        let daif = unsafe { exceptions::irq_save() };
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        while self.serving.load(Ordering::Acquire) != ticket {
            core::hint::spin_loop();
        }
        SpinGuard { lock: self, daif }
    }

}

/// Proof that this core holds the lock, and the only way to the data.
pub struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
    /// The interrupt mask state from before the lock was taken, restored on drop.
    daif: u64,
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: this guard's existence means this core is being served, so no
        // other guard for this lock exists.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as `deref`, and `&mut self` means no other borrow through this
        // guard is live either.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        // Release ordering: everything this core wrote under the lock must be
        // visible to the next core *before* it sees its ticket come up.
        self.lock.serving.fetch_add(1, Ordering::Release);
        // SAFETY: restores this core's interrupt state to what `lock` saved.
        unsafe { exceptions::irq_restore(self.daif) };
    }
}
