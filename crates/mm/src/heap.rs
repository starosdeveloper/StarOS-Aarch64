//! A small free-list heap allocator — portable, `unsafe`-contained, host-tested.
//!
//! The frame allocator ([`crate::BuddyFrameAllocator`]) hands out whole 4 KiB
//! frames; this allocator carves *arbitrary-sized* allocations out of one
//! contiguous region, so the kernel can back Rust's `alloc` (`Box`, `Vec`, …)
//! with a real heap instead of fixed-size static arrays. It is a classic
//! address-ordered free list ("hole list"): every free block holds a small header
//! in its own first bytes, and adjacent free blocks are coalesced on release so
//! the heap does not fragment into dust.
//!
//! Invariant that keeps the `unsafe` honest: the managed region, and therefore
//! every hole, is kept aligned to and a multiple of [`MIN_BLOCK`]. Requested
//! sizes are rounded up the same way and requested alignments are powers of two
//! ≥ `MIN_BLOCK` (or smaller, in which case `MIN_BLOCK` alignment already
//! satisfies them). Splitting a hole therefore always yields pieces that are
//! themselves valid `MIN_BLOCK`-aligned holes — a leftover can never be stranded
//! at a size between 1 and `MIN_BLOCK`.

use core::alloc::Layout;
use core::mem::{align_of, size_of};
use core::ptr::NonNull;

/// Minimum block size and alignment: enough to hold a [`Hole`] header. On a
/// 64-bit target this is 16 bytes (`usize` size + pointer).
pub const MIN_BLOCK: usize = size_of::<Hole>();

/// The header stored in the first bytes of every free block. `size` is the whole
/// block's byte length (header included); `next` links to the next hole in
/// ascending address order.
#[repr(C)]
pub struct Hole {
    size: usize,
    next: Option<NonNull<Hole>>,
}

/// Round `value` up to a multiple of `align` (a power of two).
const fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

/// The normalized block size an allocation of `layout` occupies: at least a
/// header, rounded up to `MIN_BLOCK`. Computed identically on `alloc` and
/// `dealloc` so the two always agree.
fn block_size(layout: Layout) -> usize {
    align_up(layout.size().max(MIN_BLOCK), MIN_BLOCK)
}

/// An address-ordered free-list allocator over a single region of memory.
pub struct FreeListAllocator {
    head: Option<NonNull<Hole>>,
}

// SAFETY: the allocator owns its region exclusively; the kernel serializes access
// (single-core, allocation happens with IRQs effectively excluded via the global
// allocator's own discipline). Sending it between contexts is sound.
unsafe impl Send for FreeListAllocator {}

impl Default for FreeListAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl FreeListAllocator {
    /// An empty allocator managing no memory. Call [`init`](Self::init) before use.
    #[must_use]
    pub const fn new() -> Self {
        Self { head: None }
    }

    /// Hand the allocator a region `[start, start + size)` to manage as one big
    /// free block.
    ///
    /// # Safety
    /// `start` must point to `size` bytes of otherwise-unused, writable memory
    /// that outlive the allocator. `start` must be `MIN_BLOCK`-aligned and `size`
    /// a multiple of `MIN_BLOCK` and at least `MIN_BLOCK`.
    pub unsafe fn init(&mut self, start: *mut u8, size: usize) {
        debug_assert!((start as usize).is_multiple_of(align_of::<Hole>()));
        debug_assert!(size >= MIN_BLOCK && size.is_multiple_of(MIN_BLOCK));
        let hole = start.cast::<Hole>();
        // SAFETY: `start` is writable and large enough to hold a header per the
        // contract; we initialize the single spanning hole.
        unsafe {
            hole.write(Hole { size, next: None });
        }
        self.head = NonNull::new(hole);
    }

    /// Allocate for `layout`, returning a suitably-aligned pointer or `None` if no
    /// hole is large enough. First-fit over the address-ordered list.
    pub fn alloc(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        let size = block_size(layout);
        let align = layout.align().max(MIN_BLOCK);

        let mut prev: Option<NonNull<Hole>> = None;
        let mut cur = self.head;
        while let Some(hole) = cur {
            // SAFETY: every node in the list is a live `Hole` we wrote.
            let (hole_addr, hole_size, hole_next) = unsafe {
                let h = hole.as_ref();
                (hole.as_ptr() as usize, h.size, h.next)
            };
            let alloc_addr = align_up(hole_addr, align);
            let front_pad = alloc_addr - hole_addr;
            let hole_end = hole_addr + hole_size;

            // Fits if the aligned allocation, plus any front padding, stays inside
            // this hole. Front pad and tail are each 0 or a whole `MIN_BLOCK` hole
            // (the alignment invariant guarantees no in-between remnant).
            if front_pad + size <= hole_size {
                let tail_addr = alloc_addr + size;
                let tail_size = hole_end - tail_addr;

                // Rebuild the list around this hole: the front padding (if any)
                // stays a hole in place; the tail (if any) becomes a new hole.
                let mut replacement = hole_next;
                if tail_size > 0 {
                    let tail = tail_addr as *mut Hole;
                    // SAFETY: `tail_addr` is inside the original hole, MIN_BLOCK
                    // aligned, with room for a header.
                    unsafe { tail.write(Hole { size: tail_size, next: hole_next }) };
                    replacement = NonNull::new(tail);
                }
                if front_pad > 0 {
                    // SAFETY: `hole` still owns `[hole_addr, alloc_addr)`.
                    unsafe { hole.as_ptr().write(Hole { size: front_pad, next: replacement }) };
                    replacement = Some(hole);
                }
                // Unlink: point whatever preceded this hole at the replacement.
                match prev {
                    // SAFETY: `p` is a live hole earlier in the list.
                    Some(p) => unsafe { (*p.as_ptr()).next = replacement },
                    None => self.head = replacement,
                }
                return NonNull::new(alloc_addr as *mut u8);
            }
            prev = cur;
            cur = hole_next;
        }
        None
    }

    /// Return the block at `ptr` (from a prior [`alloc`](Self::alloc) with the same
    /// `layout`) to the free list, coalescing with adjacent free blocks.
    ///
    /// # Safety
    /// `ptr`/`layout` must come from a matching `alloc` on this allocator and not
    /// have been freed since.
    pub unsafe fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        let addr = ptr.as_ptr() as usize;
        let size = block_size(layout);

        // Find the insertion point: the last hole whose address is below `addr`.
        let mut prev: Option<NonNull<Hole>> = None;
        let mut cur = self.head;
        while let Some(hole) = cur {
            if hole.as_ptr() as usize > addr {
                break;
            }
            prev = cur;
            // SAFETY: live hole.
            cur = unsafe { hole.as_ref().next };
        }

        let node = addr as *mut Hole;
        // SAFETY: `addr` is a freed block of `size` bytes; write its header.
        unsafe { node.write(Hole { size, next: cur }) };
        let mut node = NonNull::new(node).expect("freed pointer is non-null");

        // Link `prev -> node`, or make it the new head.
        match prev {
            // SAFETY: live hole earlier in the list.
            Some(p) => unsafe { (*p.as_ptr()).next = Some(node) },
            None => self.head = Some(node),
        }

        // Coalesce forward (node + next) then backward (prev + node).
        // SAFETY: all three are live holes at known addresses/sizes.
        unsafe {
            coalesce(node);
            if let Some(p) = prev {
                coalesce(p);
                node = p;
            }
            let _ = node;
        }
    }
}

/// If `hole` is immediately followed in memory by its list successor, merge them
/// into one block.
///
/// # Safety
/// `hole` must be a live node whose `next` (if any) is also live.
unsafe fn coalesce(hole: NonNull<Hole>) {
    // SAFETY: caller guarantees `hole` and its successor are live.
    unsafe {
        let h = hole.as_ptr();
        let Some(next) = (*h).next else { return };
        let hole_end = hole.as_ptr() as usize + (*h).size;
        if hole_end == next.as_ptr() as usize {
            (*h).size += next.as_ref().size;
            (*h).next = next.as_ref().next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::alloc::Layout;

    /// Give a `FreeListAllocator` a heap-backed region for testing.
    fn with_region(bytes: usize, f: impl FnOnce(&mut FreeListAllocator, usize)) {
        let mut backing = vec![0u8; bytes + MIN_BLOCK];
        let base = backing.as_mut_ptr();
        // Align the start up to MIN_BLOCK.
        let start = align_up(base as usize, MIN_BLOCK);
        let size = (bytes) & !(MIN_BLOCK - 1);
        let mut a = FreeListAllocator::new();
        // SAFETY: `backing` lives for the closure; region is inside it.
        unsafe { a.init(start as *mut u8, size) };
        f(&mut a, start);
        drop(backing);
    }

    #[test]
    fn alloc_returns_aligned_nonoverlapping_blocks() {
        with_region(4096, |a, _| {
            let l = Layout::from_size_align(24, 8).unwrap();
            let p1 = a.alloc(l).unwrap();
            let p2 = a.alloc(l).unwrap();
            assert!(p1.as_ptr() as usize % 8 == 0);
            assert!(p2.as_ptr() as usize % 8 == 0);
            let d = (p2.as_ptr() as usize).abs_diff(p1.as_ptr() as usize);
            assert!(d >= block_size(l), "blocks overlap: distance {d}");
        });
    }

    #[test]
    fn freed_block_is_reused() {
        with_region(4096, |a, _| {
            let l = Layout::from_size_align(64, 16).unwrap();
            let p1 = a.alloc(l).unwrap();
            // SAFETY: p1/l came from this allocator.
            unsafe { a.dealloc(p1, l) };
            let p2 = a.alloc(l).unwrap();
            assert_eq!(p1.as_ptr(), p2.as_ptr(), "freed block should be reused");
        });
    }

    #[test]
    fn coalescing_restores_a_large_allocation() {
        with_region(4096, |a, _| {
            let small = Layout::from_size_align(64, 16).unwrap();
            let a1 = a.alloc(small).unwrap();
            let a2 = a.alloc(small).unwrap();
            let a3 = a.alloc(small).unwrap();
            // Free all three; adjacent blocks must merge back together.
            // SAFETY: each pointer/layout is from this allocator, freed once.
            unsafe {
                a.dealloc(a2, small);
                a.dealloc(a1, small);
                a.dealloc(a3, small);
            }
            // A block larger than any single small allocation now fits.
            let big = Layout::from_size_align(3 * 64, 16).unwrap();
            assert!(a.alloc(big).is_some(), "coalescing should permit a large alloc");
        });
    }

    #[test]
    fn exhaustion_returns_none() {
        with_region(256, |a, _| {
            let l = Layout::from_size_align(200, 8).unwrap();
            assert!(a.alloc(l).is_some());
            // The region cannot satisfy a second 200-byte request.
            assert!(a.alloc(l).is_none());
        });
    }

    #[test]
    fn writes_do_not_corrupt_the_allocator() {
        with_region(8192, |a, _| {
            let l = Layout::from_size_align(128, 16).unwrap();
            let mut ptrs = vec![];
            for i in 0..16 {
                let p = a.alloc(l).unwrap();
                // SAFETY: freshly allocated 128 bytes we own.
                unsafe { core::ptr::write_bytes(p.as_ptr(), i as u8, 128) };
                ptrs.push(p);
            }
            // Free half, re-allocate, and confirm the survivors kept their bytes.
            for p in ptrs.iter().skip(8) {
                // SAFETY: allocated above, freed once.
                unsafe { a.dealloc(*p, l) };
            }
            for (i, p) in ptrs.iter().take(8).enumerate() {
                // SAFETY: still-live allocation.
                let byte = unsafe { *p.as_ptr() };
                assert_eq!(byte, i as u8, "live block {i} was corrupted");
            }
        });
    }
}
