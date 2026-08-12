//! Layer 2 of the contract: `malloc` and its family, over `MapAnon`.
//!
//! The kernel hands out pages and nothing smaller — `MapAnon(pages)` is the whole
//! interface — so everything a C program means by "allocate 40 bytes" lives here.
//!
//! The design is the boring one on purpose: an address-ordered doubly linked list
//! of every block in every region, each block carrying its size and whether it is
//! free. Allocation is first fit with splitting; freeing coalesces with the
//! neighbours *if they are physically adjacent*, which is what keeps two regions
//! that happen to be next to each other in the list from being merged across the
//! gap between them.
//!
//! It is deliberately not a size-class allocator with per-thread caches. That is
//! the right shape once threads exist and there is something to measure; writing it
//! now would mean a complicated allocator tuned for a workload nobody has run.
//!
//! The page source is a trait so the whole thing is testable on the host, where the
//! "pages" come from a `Vec`. Every bug this code can have — a split that loses a
//! byte, a coalesce that merges across regions, an alignment that is right only for
//! the first allocation — is a bug that shows up identically there.

use core::ptr;

/// Everything is 16-aligned: that is `max_align_t` on AArch64, so a pointer this
/// allocator returns is good for any C type including `long double` and NEON
/// vectors.
pub(crate) const ALIGN: usize = 16;

/// Bytes of bookkeeping in front of every payload. A multiple of [`ALIGN`], so a
/// 16-aligned block start yields a 16-aligned payload.
pub(crate) const HEADER: usize = 32;

/// How much to ask the kernel for when the free list cannot satisfy a request.
/// Larger than most single allocations, so a program that allocates in small pieces
/// does not make a syscall per piece.
const GROW_BYTES: usize = 64 * 1024;

/// Where memory comes from. The real implementation maps anonymous pages; the tests
/// hand out slices of a `Vec`.
pub(crate) trait Pages {
    /// Map at least `bytes` (rounded up to whole pages) and return the start, or
    /// `None` if the kernel refused.
    fn map(&mut self, bytes: usize) -> Option<*mut u8>;
    /// The page size this source deals in.
    fn page_size(&self) -> usize;
}

/// One block: header plus payload, laid out contiguously in a region.
#[repr(C)]
struct Block {
    /// Payload bytes, not counting this header.
    size: usize,
    /// Previous block by address, or null.
    prev: *mut Block,
    /// Next block by address, or null.
    next: *mut Block,
    /// Is the payload available?
    free: bool,
}

/// The allocator.
pub(crate) struct Heap<P: Pages> {
    source: P,
    /// First block by address; the list is address-ordered within each region and
    /// regions are appended in the order they were mapped.
    head: *mut Block,
    /// Last block, so growing is O(1).
    tail: *mut Block,
    /// Live payload bytes and live block count — cheap, and the only way to notice
    /// a leak in a program that has no allocator statistics of its own.
    pub(crate) live_bytes: usize,
    pub(crate) live_blocks: usize,
}

impl<P: Pages> Heap<P> {
    pub(crate) const fn new(source: P) -> Self {
        Self {
            source,
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
            live_bytes: 0,
            live_blocks: 0,
        }
    }

    /// Allocate `size` bytes, 16-aligned, or null.
    pub(crate) fn alloc(&mut self, size: usize) -> *mut u8 {
        self.alloc_aligned(size, ALIGN)
    }

    /// Allocate `size` bytes aligned to `align` (a power of two), or null.
    ///
    /// Over-alignment is served by splitting rather than by hiding an offset in
    /// front of the payload: the leading gap becomes an ordinary free block, so
    /// `free` needs to know nothing about how the pointer was obtained. A scheme
    /// that stashed the real base pointer before the payload would work until
    /// something called `realloc` on it.
    pub(crate) fn alloc_aligned(&mut self, size: usize, align: usize) -> *mut u8 {
        if size == 0 {
            // C allows null here, but a program that checks `p != NULL` and then
            // frees the result of `malloc(0)` is common enough that a unique
            // one-byte block is the friendlier answer.
            return self.alloc_aligned(1, align);
        }
        let align = align.max(ALIGN);
        let size = round_up(size, ALIGN);
        // Room for a leading gap big enough to become its own block, in the worst
        // case where the payload is one byte past an alignment boundary.
        let need = size + if align > ALIGN { align + HEADER } else { 0 };

        let mut block = self.find_free(need);
        if block.is_null() {
            if !self.grow(need + HEADER) {
                return ptr::null_mut();
            }
            block = self.find_free(need);
            if block.is_null() {
                return ptr::null_mut();
            }
        }

        // SAFETY: `find_free` returned a block of this heap with `size >= need`.
        unsafe {
            let mut block = block;
            let payload = payload_of(block);
            let aligned = round_up(payload as usize, align);
            if aligned != payload as usize {
                // Split off the leading gap. It is at least `HEADER + ALIGN` bytes
                // because `need` reserved that much.
                let gap = aligned - payload as usize - HEADER;
                block = self.split(block, gap);
            }
            self.take(block, size);
            payload_of(block)
        }
    }

    /// Release a payload pointer this heap returned.
    ///
    /// # Safety
    /// `ptr` must be null or a pointer this heap returned and has not freed since.
    pub(crate) unsafe fn free(&mut self, ptr: *mut u8) {
        if ptr.is_null() {
            return;
        }
        // SAFETY: forwarded from the caller — the header sits directly in front.
        let block = unsafe { ptr.sub(HEADER).cast::<Block>() };
        // SAFETY: as above.
        unsafe {
            debug_assert!(!(*block).free, "double free");
            (*block).free = true;
            self.live_bytes -= (*block).size;
            self.live_blocks -= 1;
            self.coalesce(block);
        }
    }

    /// Resize an allocation, moving it if it must move.
    ///
    /// # Safety
    /// As [`free`](Heap::free) for `ptr`.
    pub(crate) unsafe fn realloc(&mut self, ptr: *mut u8, size: usize) -> *mut u8 {
        if ptr.is_null() {
            return self.alloc(size);
        }
        if size == 0 {
            // SAFETY: forwarded.
            unsafe { self.free(ptr) };
            return ptr::null_mut();
        }
        // SAFETY: forwarded.
        let old = unsafe { (*ptr.sub(HEADER).cast::<Block>()).size };
        if size <= old {
            return ptr;
        }
        let new = self.alloc(size);
        if new.is_null() {
            return ptr::null_mut();
        }
        // SAFETY: `old` bytes are live in the source and `size > old` in the
        // destination; the two blocks are distinct.
        unsafe {
            ptr::copy_nonoverlapping(ptr, new, old);
            self.free(ptr);
        }
        new
    }

    /// First free block with room for `size`.
    fn find_free(&mut self, size: usize) -> *mut Block {
        let mut block = self.head;
        while !block.is_null() {
            // SAFETY: every block in the list is one this heap created.
            unsafe {
                if (*block).free && (*block).size >= size {
                    return block;
                }
                block = (*block).next;
            }
        }
        ptr::null_mut()
    }

    /// Mark `block` used, splitting off the remainder if it is worth a block.
    ///
    /// # Safety
    /// `block` must be a free block of this heap with `size >= want`.
    unsafe fn take(&mut self, block: *mut Block, want: usize) {
        // SAFETY: forwarded.
        unsafe {
            if (*block).size >= want + HEADER + ALIGN {
                self.split(block, want);
            }
            (*block).free = false;
            self.live_bytes += (*block).size;
            self.live_blocks += 1;
        }
    }

    /// Split `block` so its payload becomes `first_size` bytes, and return the
    /// *second* block, which is free and holds the rest.
    ///
    /// # Safety
    /// `block` must be free with `size >= first_size + HEADER + ALIGN`.
    unsafe fn split(&mut self, block: *mut Block, first_size: usize) -> *mut Block {
        // SAFETY: forwarded.
        unsafe {
            let rest = (*block).size - first_size - HEADER;
            let second = payload_of(block).add(first_size).cast::<Block>();
            (*second).size = rest;
            (*second).free = true;
            (*second).prev = block;
            (*second).next = (*block).next;
            if !(*block).next.is_null() {
                (*(*block).next).prev = second;
            } else {
                self.tail = second;
            }
            (*block).size = first_size;
            (*block).next = second;
            second
        }
    }

    /// Merge `block` with its neighbours where they are free *and adjacent*.
    ///
    /// # Safety
    /// `block` must be a free block of this heap.
    unsafe fn coalesce(&mut self, block: *mut Block) {
        // SAFETY: forwarded.
        unsafe {
            let next = (*block).next;
            if !next.is_null() && (*next).free && adjacent(block, next) {
                (*block).size += HEADER + (*next).size;
                (*block).next = (*next).next;
                if !(*next).next.is_null() {
                    (*(*next).next).prev = block;
                } else {
                    self.tail = block;
                }
            }
            let prev = (*block).prev;
            if !prev.is_null() && (*prev).free && adjacent(prev, block) {
                (*prev).size += HEADER + (*block).size;
                (*prev).next = (*block).next;
                if !(*block).next.is_null() {
                    (*(*block).next).prev = prev;
                } else {
                    self.tail = prev;
                }
            }
        }
    }

    /// Ask the page source for another region and append it as one free block.
    fn grow(&mut self, need: usize) -> bool {
        let page = self.source.page_size();
        let bytes = round_up(need.max(GROW_BYTES), page);
        let Some(base) = self.source.map(bytes) else {
            return false;
        };
        debug_assert_eq!(base as usize % ALIGN, 0, "a mapped region must be aligned");
        // SAFETY: the source just handed us `bytes` of writable memory.
        unsafe {
            let block = base.cast::<Block>();
            (*block).size = bytes - HEADER;
            (*block).free = true;
            (*block).prev = self.tail;
            (*block).next = ptr::null_mut();
            if self.tail.is_null() {
                self.head = block;
            } else {
                (*self.tail).next = block;
            }
            self.tail = block;
        }
        true
    }

    /// Count the blocks, for tests and for a leak report.
    #[cfg(test)]
    fn blocks(&self) -> (usize, usize) {
        let (mut free, mut used) = (0, 0);
        let mut block = self.head;
        while !block.is_null() {
            // SAFETY: list invariant.
            unsafe {
                if (*block).free {
                    free += 1;
                } else {
                    used += 1;
                }
                block = (*block).next;
            }
        }
        (free, used)
    }
}

/// The payload that follows a header.
///
/// # Safety
/// `block` must be a block of a heap.
unsafe fn payload_of(block: *mut Block) -> *mut u8 {
    // SAFETY: forwarded.
    unsafe { block.cast::<u8>().add(HEADER) }
}

/// Do these two blocks touch? Blocks from different regions can be neighbours in
/// the list without being neighbours in memory, and merging those would hand out a
/// range that crosses a hole.
///
/// # Safety
/// Both must be blocks of a heap.
unsafe fn adjacent(first: *mut Block, second: *mut Block) -> bool {
    // SAFETY: forwarded.
    unsafe { payload_of(first).add((*first).size) == second.cast::<u8>() }
}

pub(crate) const fn round_up(value: usize, to: usize) -> usize {
    value.div_ceil(to) * to
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate alloc;
    use alloc::boxed::Box;
    use alloc::vec::Vec;

    /// A page source backed by leaked boxes, so addresses stay valid and regions
    /// are *not* adjacent — which is the interesting case for coalescing.
    struct FakePages {
        regions: Vec<&'static mut [u8]>,
    }

    impl Pages for FakePages {
        fn map(&mut self, bytes: usize) -> Option<*mut u8> {
            let region: &'static mut [u8] = Box::leak(alloc::vec![0u8; bytes + ALIGN].into_boxed_slice());
            let base = region.as_mut_ptr();
            let offset = base.align_offset(ALIGN);
            self.regions.push(region);
            // SAFETY: the allocation is `bytes + ALIGN` long, so this stays inside.
            Some(unsafe { base.add(offset) })
        }
        fn page_size(&self) -> usize {
            4096
        }
    }

    fn heap() -> Heap<FakePages> {
        Heap::new(FakePages { regions: Vec::new() })
    }

    #[test]
    fn allocations_are_aligned_and_distinct() {
        let mut h = heap();
        let a = h.alloc(1);
        let b = h.alloc(17);
        let c = h.alloc(4096);
        for p in [a, b, c] {
            assert!(!p.is_null());
            assert_eq!(p as usize % ALIGN, 0, "every payload is max_align_t-aligned");
        }
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_eq!(h.live_blocks, 3);
    }

    #[test]
    fn freed_memory_is_reused() {
        let mut h = heap();
        let a = h.alloc(64);
        // SAFETY: `a` came from this heap.
        unsafe { h.free(a) };
        let b = h.alloc(64);
        assert_eq!(a, b, "a free block of the right size must be reused, not grown past");
        assert_eq!(h.live_bytes, 64);
    }

    #[test]
    fn adjacent_frees_coalesce() {
        let mut h = heap();
        let a = h.alloc(64);
        let b = h.alloc(64);
        let c = h.alloc(64);
        // SAFETY: all three came from this heap.
        unsafe {
            h.free(a);
            h.free(b);
        }
        // 64 + header + 64 must now be one block, so an allocation larger than
        // either piece fits without growing.
        let big = h.alloc(64 + HEADER + 64);
        assert_eq!(big, a, "the two free blocks were not merged");
        // SAFETY: still live.
        unsafe {
            h.free(c);
            h.free(big);
        }
        assert_eq!(h.live_bytes, 0);
        assert_eq!(h.live_blocks, 0);
    }

    #[test]
    fn regions_do_not_coalesce_across_the_gap() {
        // Two regions from the page source are not adjacent in memory. Merging them
        // because they are neighbours in the *list* would hand out a range that
        // spans a hole — memory that was never mapped.
        let mut h = heap();
        let big = 60 * 1024;
        let a = h.alloc(big);
        let b = h.alloc(big); // forces a second region
        // SAFETY: both came from this heap.
        unsafe {
            h.free(a);
            h.free(b);
        }
        let (free, used) = h.blocks();
        assert_eq!(used, 0);
        assert!(free >= 2, "blocks from two regions must stay separate, got {free}");
    }

    #[test]
    fn splitting_leaves_the_remainder_usable() {
        let mut h = heap();
        let big = h.alloc(4096);
        // SAFETY: from this heap.
        unsafe { h.free(big) };
        let small = h.alloc(16);
        let second = h.alloc(16);
        assert_eq!(small, big);
        assert_eq!(second, unsafe { small.add(16 + HEADER) }, "the split remainder is next in memory");
    }

    #[test]
    fn over_alignment_is_a_real_split() {
        let mut h = heap();
        let _pad = h.alloc(24); // push the next payload off a 256 boundary
        let p = h.alloc_aligned(100, 256);
        assert_eq!(p as usize % 256, 0);
        // The gap in front became an ordinary block, so freeing works with no
        // knowledge of how the pointer was obtained.
        // SAFETY: from this heap.
        unsafe { h.free(p) };
        assert_eq!(h.live_blocks, 1);
    }

    #[test]
    fn realloc_grows_and_copies() {
        let mut h = heap();
        let p = h.alloc(16);
        // SAFETY: 16 bytes are live.
        unsafe { ptr::write_bytes(p, 0xAB, 16) };
        // SAFETY: `p` is from this heap.
        let q = unsafe { h.realloc(p, 4096) };
        assert!(!q.is_null());
        // SAFETY: at least 16 bytes were copied into a 4096-byte block.
        let copied = unsafe { core::slice::from_raw_parts(q, 16) };
        assert!(copied.iter().all(|&b| b == 0xAB), "realloc lost the old contents");
        // SAFETY: shrinking in place returns the same pointer.
        assert_eq!(unsafe { h.realloc(q, 8) }, q);
    }

    #[test]
    fn exhaustion_returns_null_rather_than_panicking() {
        struct NoPages;
        impl Pages for NoPages {
            fn map(&mut self, _bytes: usize) -> Option<*mut u8> {
                None
            }
            fn page_size(&self) -> usize {
                4096
            }
        }
        let mut h = Heap::new(NoPages);
        assert!(h.alloc(16).is_null());
    }
}
