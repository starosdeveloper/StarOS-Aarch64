//! Portable memory-management core.
//!
//! Address arithmetic and the frame allocator are pure logic with no MMIO, so
//! they live in their own crate and are fully unit-testable on the host. The
//! arch crate supplies the page-table walk that actually installs mappings; the
//! *decisions* about which frames to hand out are made here.
//!
//! `no_std` except under `cargo test`, where the host test harness needs `std`.
#![cfg_attr(not(test), no_std)]

// The buddy allocator's tree is sized at runtime from what the device tree
// reports, so it lives on the kernel heap — which `crate::heap` itself backs.
// The two are not circular: the heap is a region of raw bytes handed to it by
// the caller, and only the *frame* allocator's metadata comes from it.
extern crate alloc;

pub mod heap;
pub mod region;

use staros_abi::error::{KError, KResult};

/// Size of a base page/frame in bytes (4 KiB).
pub const PAGE_SIZE: usize = 4096;

/// A physical address. Distinct from [`VirtAddr`] at the type level so the two
/// can never be confused — a class of bug the old flat `usize` addressing
/// invited.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(transparent)]
pub struct PhysAddr(pub usize);

/// A virtual address.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(transparent)]
pub struct VirtAddr(pub usize);

impl PhysAddr {
    /// Returns `true` if this address is aligned to a page boundary.
    #[must_use]
    pub const fn is_page_aligned(self) -> bool {
        self.0.is_multiple_of(PAGE_SIZE)
    }
}

/// Something that hands out physical frames one page at a time.
pub trait FrameAllocator {
    /// Allocate one physical frame, or `None` when memory is exhausted.
    fn allocate(&mut self) -> Option<PhysAddr>;

    /// Return a previously allocated frame to the pool.
    fn free(&mut self, frame: PhysAddr);
}

/// A minimal bump allocator over a contiguous, page-aligned physical region.
///
/// It never reclaims freed frames — it is the allocator used during early boot
/// before the real buddy allocator is online. Small, obvious, and correct.
#[derive(Debug)]
pub struct BumpFrameAllocator {
    next: usize,
    end: usize,
}

impl BumpFrameAllocator {
    /// Create an allocator covering `[start, start + len)`.
    ///
    /// # Errors
    /// Returns [`KError::InvalidArgument`] if `start` is not page-aligned.
    pub fn new(start: PhysAddr, len: usize) -> KResult<Self> {
        if !start.is_page_aligned() {
            return Err(KError::InvalidArgument);
        }
        Ok(Self {
            next: start.0,
            end: start.0 + (len / PAGE_SIZE) * PAGE_SIZE,
        })
    }
}

impl FrameAllocator for BumpFrameAllocator {
    fn allocate(&mut self) -> Option<PhysAddr> {
        if self.next >= self.end {
            return None;
        }
        let frame = PhysAddr(self.next);
        self.next += PAGE_SIZE;
        Some(frame)
    }

    fn free(&mut self, _frame: PhysAddr) {
        // Bump allocation does not reclaim; see `BuddyFrameAllocator`.
    }
}

/// A binary **buddy** frame allocator: hands out power-of-two runs of frames and,
/// crucially, *reclaims* them — freeing a block coalesces it with its buddy so
/// the memory becomes available again. This is what lets an address space be torn
/// down and its frames reused, which the bump allocator could never do.
///
/// The implementation is the classic "longest-free-run per subtree" tree: for
/// each node, `longest` records the size (in frames, a power of two) of the
/// largest free block anywhere in that node's subtree, or `0` when the node is
/// fully allocated. Allocation walks down choosing the tightest child that still
/// fits; freeing walks up merging buddies.
///
/// The tree lives on the kernel heap, so the amount of RAM this can manage is a
/// runtime fact — which it must be, since only the device tree knows how much
/// there is. The cost is [`BuddyFrameAllocator::metadata_bytes`]: 8 bytes per
/// frame, i.e. 0.2% of the memory managed. (For scale, Linux's `struct page` is
/// an order of magnitude more.)
pub struct BuddyFrameAllocator {
    /// Physical base address of frame 0.
    base: usize,
    /// Number of managed frames (a power of two).
    frames: usize,
    /// Largest free run (in frames) within each node's subtree; `0` = allocated.
    longest: alloc::boxed::Box<[u32]>,
}

impl BuddyFrameAllocator {
    /// Bytes of heap the tree for a region of `len` bytes will occupy.
    ///
    /// Callers need this *before* building the allocator: the heap has to be
    /// carved out of the same memory map, and it must be big enough to hold
    /// this. Over-estimates are safe; this is exact.
    #[must_use]
    pub const fn metadata_bytes(len: usize) -> usize {
        let frames = prev_power_of_two(len / PAGE_SIZE);
        if frames == 0 {
            return 0;
        }
        (2 * frames - 1) * size_of::<u32>()
    }

    /// Build an allocator over `[start, start + len)`. The region is measured in
    /// whole frames and rounded *down* to a power of two (any tail beyond that is
    /// left unmanaged).
    ///
    /// # Errors
    /// Returns [`KError::InvalidArgument`] if `start` is not page-aligned or the
    /// region holds no whole frame, and [`KError::OutOfResources`] if the heap
    /// cannot hold the tree ([`metadata_bytes`](Self::metadata_bytes)).
    pub fn new(start: PhysAddr, len: usize) -> KResult<Self> {
        if !start.is_page_aligned() {
            return Err(KError::InvalidArgument);
        }
        let avail = len / PAGE_SIZE;
        let frames = prev_power_of_two(avail);
        if frames == 0 {
            return Err(KError::InvalidArgument);
        }
        // Build the tree without ever materialising it on the stack: for a
        // multi-gigabyte pool this is megabytes.
        let nodes = 2 * frames - 1;
        let mut longest = alloc::vec::Vec::new();
        longest
            .try_reserve_exact(nodes)
            .map_err(|_| KError::OutOfResources)?;
        let mut node_size = 2 * frames;
        for i in 0..nodes {
            if (i + 1).is_power_of_two() {
                node_size /= 2;
            }
            longest.push(node_size as u32);
        }
        Ok(Self {
            base: start.0,
            frames,
            longest: longest.into_boxed_slice(),
        })
    }

    /// Number of frames under management.
    #[must_use]
    pub const fn frames(&self) -> usize {
        self.frames
    }

    /// The longest run of contiguous free frames, which the root node tracks by
    /// construction.
    ///
    /// This is the cheapest honest answer to "did everything come back?". It
    /// equals [`frames`](Self::frames) exactly when the pool is entirely free
    /// *and* fully coalesced — one leaked frame anywhere splits the run and drops
    /// this number, however much is free in total. A caller that records it before
    /// handing memory out and compares afterwards has checked reclamation without
    /// the allocator having to track owners.
    #[must_use]
    pub fn largest_free_run(&self) -> usize {
        self.longest[0] as usize
    }

    /// Allocate a run of `count` contiguous frames (rounded up to a power of two),
    /// returning the base address, or `None` if no block is large enough.
    pub fn alloc_pages(&mut self, count: usize) -> Option<PhysAddr> {
        let size = count.max(1).next_power_of_two();
        if (self.longest[0] as usize) < size {
            return None;
        }
        // Walk down to a node of exactly `size`, always taking the tightest child
        // that still fits so large blocks stay intact.
        let mut index = 0usize;
        let mut node_size = self.frames;
        while node_size != size {
            let left = 2 * index + 1;
            let right = 2 * index + 2;
            let lfit = self.longest[left] as usize >= size;
            let rfit = self.longest[right] as usize >= size;
            index = if lfit && (!rfit || self.longest[left] <= self.longest[right]) {
                left
            } else {
                right
            };
            node_size /= 2;
        }
        self.longest[index] = 0;
        let offset = (index + 1) * node_size - self.frames;
        // Propagate the new (reduced) free run up to the root.
        while index > 0 {
            index = (index - 1) / 2;
            let left = self.longest[2 * index + 1];
            let right = self.longest[2 * index + 2];
            self.longest[index] = left.max(right);
        }
        Some(PhysAddr(self.base + offset * PAGE_SIZE))
    }

    /// Free a block previously returned by [`alloc_pages`](Self::alloc_pages) or
    /// [`allocate`](FrameAllocator::allocate). The block's size is recovered from
    /// the tree, and buddies are coalesced on the way up. A double free (the block
    /// is already free) is ignored.
    pub fn free_pages(&mut self, frame: PhysAddr) {
        if frame.0 < self.base {
            return;
        }
        let offset = (frame.0 - self.base) / PAGE_SIZE;
        if offset >= self.frames {
            return;
        }
        // Climb from the leaf to the allocated node covering this offset.
        let mut node_size = 1usize;
        let mut index = offset + self.frames - 1;
        while self.longest[index] != 0 {
            node_size *= 2;
            if index == 0 {
                return; // reached the root without finding an allocation
            }
            index = (index - 1) / 2;
        }
        self.longest[index] = node_size as u32;
        // Merge with the buddy while both halves are fully free.
        while index > 0 {
            index = (index - 1) / 2;
            node_size *= 2;
            let left = self.longest[2 * index + 1] as usize;
            let right = self.longest[2 * index + 2] as usize;
            self.longest[index] = if left + right == node_size {
                node_size as u32
            } else {
                left.max(right) as u32
            };
        }
    }
}

impl FrameAllocator for BuddyFrameAllocator {
    fn allocate(&mut self) -> Option<PhysAddr> {
        self.alloc_pages(1)
    }

    fn free(&mut self, frame: PhysAddr) {
        self.free_pages(frame);
    }
}

/// Largest power of two `<= n` (and `0` for `n == 0`).
const fn prev_power_of_two(n: usize) -> usize {
    if n == 0 {
        0
    } else {
        // Highest set bit of `n`.
        1usize << (usize::BITS - 1 - n.leading_zeros())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hands_out_aligned_frames_then_exhausts() {
        let mut alloc =
            BumpFrameAllocator::new(PhysAddr(0x4000_0000), 2 * PAGE_SIZE).unwrap();
        let a = alloc.allocate().unwrap();
        let b = alloc.allocate().unwrap();
        assert!(a.is_page_aligned() && b.is_page_aligned());
        assert_eq!(b.0 - a.0, PAGE_SIZE);
        assert_eq!(alloc.allocate(), None);
    }

    #[test]
    fn rejects_unaligned_base() {
        assert_eq!(
            BumpFrameAllocator::new(PhysAddr(0x4000_0001), PAGE_SIZE).unwrap_err(),
            KError::InvalidArgument
        );
    }

    #[test]
    fn buddy_reclaims_and_reuses_a_frame() {
        let mut b = BuddyFrameAllocator::new(PhysAddr(0x4000_0000), 8 * PAGE_SIZE).unwrap();
        let a = b.allocate().unwrap();
        let c = b.allocate().unwrap();
        assert_ne!(a, c);
        // Freeing a frame must let a later allocation hand it back — the whole
        // point of a reclaiming allocator (the bump allocator cannot do this).
        b.free(a);
        let reused = b.allocate().unwrap();
        assert_eq!(reused, a);
        // The unrelated live frame is never handed out again.
        let d = b.allocate().unwrap();
        assert_ne!(d, c);
    }

    #[test]
    fn buddy_largest_free_run_detects_a_leak() {
        // The kernel uses this to check that a torn-down address space gave every
        // frame back, so it has to be sensitive to a *single* missing one.
        let mut b = BuddyFrameAllocator::new(PhysAddr(0x4000_0000), 8 * PAGE_SIZE).unwrap();
        assert_eq!(b.largest_free_run(), 8, "a fresh pool is entirely free");

        let f: [_; 8] = core::array::from_fn(|_| b.allocate().unwrap());
        assert_eq!(b.largest_free_run(), 0, "nothing is free once all 8 are out");

        // Give back all but one — the one leak the check must not miss.
        for frame in &f[..7] {
            b.free(*frame);
        }
        assert!(
            b.largest_free_run() < 8,
            "7 of 8 frames free must not read as a whole pool",
        );

        b.free(f[7]);
        assert_eq!(b.largest_free_run(), 8, "the last frame back restores the run");
    }

    #[test]
    fn buddy_coalesces_buddies_into_a_larger_block() {
        // Four frames: allocate all four as singles, then free them all. If
        // coalescing works the tree is whole again and a 4-frame run succeeds.
        let mut b = BuddyFrameAllocator::new(PhysAddr(0x4000_0000), 4 * PAGE_SIZE).unwrap();
        let f: [_; 4] = core::array::from_fn(|_| b.allocate().unwrap());
        assert!(b.alloc_pages(4).is_none(), "pool should be exhausted");
        for frame in f {
            b.free(frame);
        }
        let big = b.alloc_pages(4).expect("coalesced back into one 4-frame block");
        assert_eq!(big, PhysAddr(0x4000_0000));
    }

    #[test]
    fn buddy_allocates_aligned_power_of_two_runs() {
        let mut b = BuddyFrameAllocator::new(PhysAddr(0x4000_0000), 8 * PAGE_SIZE).unwrap();
        let two = b.alloc_pages(2).unwrap();
        // A 2-frame run is 2-frame aligned; its buddy split leaves the rest usable.
        assert!(two.0.is_multiple_of(2 * PAGE_SIZE));
        let one = b.allocate().unwrap();
        assert_ne!(one, two);
        b.free_pages(two);
        let two_again = b.alloc_pages(2).unwrap();
        assert_eq!(two_again, two);
    }

    #[test]
    fn buddy_exhausts_then_recovers() {
        let mut b = BuddyFrameAllocator::new(PhysAddr(0x4000_0000), 2 * PAGE_SIZE).unwrap();
        let a = b.allocate().unwrap();
        let c = b.allocate().unwrap();
        assert_eq!(b.allocate(), None);
        b.free(c);
        assert_eq!(b.allocate(), Some(c));
        b.free(a);
        b.free(c);
        // Fully free again: the whole 2-frame pool coalesces.
        assert!(b.alloc_pages(2).is_some());
    }
}
