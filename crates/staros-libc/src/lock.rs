//! The lock the library's own statics are behind.
//!
//! Layers 1–4 were written single-threaded and said so. Layer 5 made that a bug
//! rather than a note: the heap, the console's line buffer and the file-descriptor
//! table are process-wide, and the first `malloc` from two threads at once is a
//! race. This is the smallest thing that fixes it.
//!
//! It spins with `Yield` rather than parking. Parking would need a notification per
//! waiter, and the parkers belong to the thread layer, which is itself a user of
//! this lock — the dependency would run in a circle. The regions guarded here are a
//! few dozen instructions of list surgery, with one exception that is deliberate
//! and documented at its use site: the file layer holds its lock across the IPC to
//! the file server, because there is one shared buffer and one reply endpoint to
//! go round.

use core::sync::atomic::{AtomicU32, Ordering};

/// A spin lock, unlocked when zero.
pub(crate) struct Spin {
    state: AtomicU32,
}

impl Spin {
    pub(crate) const fn new() -> Self {
        Self { state: AtomicU32::new(0) }
    }

    /// Take the lock, yielding the CPU while somebody else holds it.
    ///
    /// Yielding matters more here than on a machine with spare cores: this system
    /// preempts, so a spinner that never yields can hold a whole timeslice against
    /// a lock holder that has been preempted, and on one core that is a livelock
    /// until the tick.
    pub(crate) fn lock(&self) -> Guard<'_> {
        while self
            .state
            .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            crate::sys::yield_now();
        }
        Guard { state: &self.state }
    }
}

/// Releases the lock when dropped.
pub(crate) struct Guard<'a> {
    state: &'a AtomicU32,
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.state.store(0, Ordering::Release);
    }
}
