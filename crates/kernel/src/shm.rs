//! The frames behind a shared-memory object.
//!
//! [`Object::SharedMemory`](crate::obj::Object::SharedMemory) used to be one
//! `(phys, pages)` pair, which said something the hardware never asked for: that
//! every shared buffer is one physically contiguous run. Nothing about two
//! processes reading the same pages through their own tables requires that — only
//! DMA does, where a device walks physical addresses with no tables of its own.
//!
//! The cost was a refusal nobody could see the reason for. Contiguous allocation
//! comes out of a buddy tree, so it rounds up to a power of two and needs a single
//! unbroken block: a pool with sixty megabytes free in scattered pieces would
//! refuse eight, and the caller got `OutOfResources` from a machine that plainly
//! had the memory. A full-screen backing store is exactly the size where that
//! starts happening, and "sometimes the window will not open" is the worst shape a
//! bug can have.
//!
//! So the frames live here, as a list, and the object holds an index into this
//! table. The indirection exists because [`Object`](crate::obj::Object) is `Copy`
//! and lives in a fixed-size slot — a `Vec` cannot go inside it — and because the
//! lifetime of the frames is not the lifetime of the capability: revoking a
//! capability stops it resolving, while the frames are reclaimed once, at shutdown,
//! when no task can still be mapping them.

use alloc::vec::Vec;

use staros_mm::{FrameAllocator, FramePool, PhysAddr, PAGE_SIZE};

use crate::sync::SpinLock;

/// One shared buffer: the physical frames it is made of, in the order they are
/// mapped. Empty means the slot is free.
///
/// Order matters and is the whole point of keeping a list: a client sees these as
/// one flat buffer, so frame *n* of this vector must land at offset `n * 4096` in
/// every task that maps it. Two tasks disagreeing about that would read each
/// other's pixels off by a page, which looks like a rendering bug and is not one.
type Run = Vec<u64>;

/// Every live shared run. Indexed by the id an object carries.
///
/// Grows on demand and reuses freed slots, for the same reason the object table
/// does: how many buffers a system needs is a property of what runs on it.
static RUNS: SpinLock<Vec<Run>> = SpinLock::new(Vec::new());

/// Allocate `pages` zeroed frames, remember them as one run, and return its id.
///
/// The frames are taken **one at a time**. That is the change: a run is a list, so
/// it can be assembled out of whatever the pool still has, and a request fails only
/// when the memory genuinely is not there rather than when it is not in one piece.
///
/// Every page is zeroed, not just the first. Memory handed to a second task must
/// not carry what a previous owner left in it, and the page nobody routinely reads
/// is exactly where a stale secret survives.
pub fn create(pages: usize) -> Option<usize> {
    let mut run = Run::new();
    if run.try_reserve_exact(pages).is_err() {
        return None;
    }

    for _ in 0..pages {
        let Some(frame) = crate::mem::with(FramePool::allocate) else {
            // Out partway: give back what this call took. Leaving them allocated
            // would be a leak that only shows up under memory pressure, which is
            // precisely when it can least be afforded.
            crate::mem::with(|frames| {
                for &f in &run {
                    frames.free(PhysAddr(f as usize));
                }
            });
            return None;
        };
        // SAFETY: the frame is in the kernel's linear map and uniquely ours until
        // the run is published below.
        unsafe {
            let va = staros_arch_aarch64::mmu::phys_to_virt(frame.0 as u64) as *mut u8;
            core::ptr::write_bytes(va, 0, PAGE_SIZE);
        }
        run.push(frame.0 as u64);
    }

    let mut runs = RUNS.lock();
    if let Some(id) = runs.iter().position(Vec::is_empty) {
        runs[id] = run;
        return Some(id);
    }
    if runs.try_reserve(1).is_err() {
        // The table cannot grow, so the frames have nowhere to be recorded. Drop the
        // lock before returning them: freeing takes the frame pool, and taking two
        // locks in one order here and the other order elsewhere is how a kernel
        // deadlocks.
        drop(runs);
        crate::mem::with(|frames| {
            for &f in &run {
                frames.free(PhysAddr(f as usize));
            }
        });
        return None;
    }
    runs.push(run);
    Some(runs.len() - 1)
}

/// Call `f` with the run's frames. `None` if the id names nothing.
///
/// A callback rather than a returned slice, because the frames live under the lock
/// and handing out a reference to them would mean handing out the lock. The caller
/// maps them, which takes the *frame* lock — so this must not be held across
/// anything that also takes the object table's, and it is not: mapping needs only
/// the pool.
pub fn with_frames<T>(id: usize, f: impl FnOnce(&[u64]) -> T) -> Option<T> {
    let runs = RUNS.lock();
    let run = runs.get(id)?;
    if run.is_empty() {
        return None;
    }
    Some(f(run))
}

/// Return a run's frames to the pool and free its slot. Idempotent.
pub fn free(id: usize) {
    crate::mem::with(|frames| free_with(id, frames));
}

/// As [`free`], for a caller that already holds the frame pool.
///
/// Both forms exist because the shutdown sweep runs *inside* `mem::with` — it is
/// handed the pool — and calling the locking form from there would take a spin lock
/// this thread already holds. That deadlock would happen once per boot, on the last
/// thing the kernel does, which is the least likely place to notice it and the
/// worst place to have it.
pub fn free_with(id: usize, frames: &mut FramePool) {
    let run = {
        let mut runs = RUNS.lock();
        match runs.get_mut(id) {
            Some(run) => core::mem::take(run),
            None => return,
        }
    };
    for &frame in &run {
        frames.free(PhysAddr(frame as usize));
    }
}
