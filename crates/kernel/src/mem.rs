//! The kernel's global physical frame allocator.
//!
//! Address spaces are built and torn down from several places — `kmain` at
//! bring-up, `sched::exit` when a task ends — so the allocator can't be a local
//! owned by `kmain`. It lives here as a single `static`, a reclaiming
//! [`BuddyFrameAllocator`] so freed frames (from a torn-down [`AddressSpace`])
//! become available again.
//!
//! Access goes through a [`SpinLock`]: any core may allocate, and the buddy
//! tree's invariants live across the whole of a split or a coalesce. [`with`]
//! hands out the borrow, and holds the lock for exactly as long as `f` runs —
//! which is why `f` must not context switch or take another kernel lock.

use staros_mm::{FramePool, PhysAddr};

use crate::sync::SpinLock;

/// The allocator, or `None` until [`init`] runs.
static FRAMES: SpinLock<Option<FramePool>> = SpinLock::new(None);

/// Initialize the global allocator over the physical region `[start, start+len)`.
/// Call exactly once, before any allocation.
///
/// A [`FramePool`] rather than a bare [`staros_mm::BuddyFrameAllocator`], and the
/// difference is what happens to a region that is not a power of two: a buddy
/// tree rounds *down*, so a 3 GiB region became 2 GiB with no message. QEMU's
/// `virt` machine happens to be given 256 MiB here, which is a power of two
/// exactly, so this tree never paid for it — a Raspberry Pi with 4 or 8 GiB will,
/// and the fix belongs in before it is needed rather than after.
///
/// # Errors
/// Propagates [`FramePool::add`] failures (misaligned base, or no heap for the
/// trees).
pub fn init(start: PhysAddr, len: usize) -> Result<(), staros_abi::error::KError> {
    let mut pool = FramePool::new();
    pool.add(start, len)?;
    *FRAMES.lock() = Some(pool);
    Ok(())
}

/// Run `f` with exclusive access to the allocator. Panics if [`init`] has not
/// run.
///
/// Holds the frame lock for the duration, so `f` must not context switch, block,
/// or reach for another kernel lock.
pub fn with<R>(f: impl FnOnce(&mut FramePool) -> R) -> R {
    let mut slot = FRAMES.lock();
    let alloc = slot.as_mut().expect("frame allocator used before init");
    f(alloc)
}

/// Allocate one frame, or `None` when memory is exhausted.
pub fn alloc_frame() -> Option<PhysAddr> {
    with(|a| a.alloc_pages(1))
}
