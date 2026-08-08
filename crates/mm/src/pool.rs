//! A frame pool over memory that is neither contiguous nor a power of two.
//!
//! [`BuddyFrameAllocator`](crate::BuddyFrameAllocator) manages one run of frames
//! rounded **down** to a power of two, which is what a buddy tree is: a complete
//! binary tree has a power-of-two number of leaves or it is not complete. That is
//! a correct allocator and an expensive way to own a machine.
//!
//! Two losses come out of it, and on a real machine they compound:
//!
//! * **The tail.** A run of 1956 MiB rounds down to 1024. Everything from 1025
//!   upwards is simply not managed — 48% of that run, gone, with no message.
//! * **The other runs.** Physical memory is not one block. A PC splits RAM either
//!   side of the PCI hole, the kernel image and the boot data sit inside usable
//!   ranges and have to be carved out, and firmware describes a dozen small
//!   reserved fragments in the low megabyte. Taking "the largest free run" and
//!   discarding the rest threw away the half of a 4 GiB machine that lives above
//!   4 GiB.
//!
//! Together those turned a guest reporting 4042 MiB usable into a kernel managing
//! 1024 MiB. Which was *asserted* by the boot matrix — the number was visible,
//! and being visible is not the same as being acceptable.
//!
//! ## What this does instead
//! A pool is a list of buddy trees. Each run handed to [`FramePool::add`] is
//! decomposed into its binary expansion — 1956 frames becomes 1024 + 512 + 256 +
//! 128 + 32 + 4 — and each piece gets a tree of its own. Nothing is rounded away,
//! because every piece *is* a power of two by construction.
//!
//! The cost is metadata: 8 bytes per frame over roughly four times as many
//! frames, so the tree bill goes from 0.2% of a quarter of the machine to 0.2% of
//! all of it. [`FramePool::metadata_bytes`] computes it exactly, and the caller
//! has to, because the heap holding those trees is carved from the same memory.
//!
//! ## What it deliberately does not do
//! No allocation across trees. A request for eight contiguous frames is answered
//! by one tree or refused, never stitched together out of two — the frames would
//! not be contiguous, and "contiguous" is the only reason to ask for more than
//! one at a time. Physical fragmentation is therefore real here, and visible:
//! [`FramePool::largest_free_run`] is the largest run *any single tree* can offer.

use alloc::vec::Vec;

use staros_abi::error::{KError, KResult};

use crate::{BuddyFrameAllocator, FrameAllocator, PhysAddr, PAGE_SIZE};

/// A frame allocator over an arbitrary set of physical runs.
pub struct FramePool {
    /// One buddy tree per power-of-two piece, in the order they were added.
    trees: Vec<BuddyFrameAllocator>,
}

impl FramePool {
    /// An empty pool. Add runs with [`FramePool::add`].
    #[must_use]
    pub const fn new() -> Self {
        Self { trees: Vec::new() }
    }

    /// Bytes of heap the trees for a run of `len` bytes will occupy.
    ///
    /// Exact, and it has to be: the heap is carved out of the same memory map
    /// before the pool exists, so getting this wrong either wastes memory or
    /// fails an allocation in a kernel with nowhere to fail to.
    ///
    /// The sum over the binary expansion. A tree of `n` leaves has `2n - 1`
    /// nodes, so the total is `2 * frames - pieces` nodes — a little less than
    /// twice the frame count, and roughly four times what the single rounded-down
    /// tree cost for the same run.
    #[must_use]
    pub const fn metadata_bytes(len: usize) -> usize {
        let mut frames = len / PAGE_SIZE;
        let mut nodes = 0usize;
        while frames > 0 {
            let piece = prev_power_of_two(frames);
            nodes += 2 * piece - 1;
            frames -= piece;
        }
        nodes * size_of::<u32>()
    }

    /// Bytes of heap the trees for all of `runs` will occupy.
    #[must_use]
    pub fn metadata_bytes_for(runs: &[(usize, usize)]) -> usize {
        runs.iter().map(|&(_, len)| Self::metadata_bytes(len)).sum()
    }

    /// Add the run `[start, start + len)`, decomposed into power-of-two pieces.
    ///
    /// Returns how many frames were taken under management, which is every whole
    /// frame in the run — the point of the decomposition.
    ///
    /// # Errors
    /// [`KError::InvalidArgument`] if `start` is not page-aligned.
    /// [`KError::OutOfResources`] if the heap cannot hold the trees.
    pub fn add(&mut self, start: PhysAddr, len: usize) -> KResult<usize> {
        if !start.is_page_aligned() {
            return Err(KError::InvalidArgument);
        }
        let mut offset = 0usize;
        let mut remaining = len / PAGE_SIZE;
        let mut added = 0usize;
        // Largest piece first. Not cosmetic: it means the biggest tree covers the
        // lowest addresses of the run, so a request for a large contiguous block
        // is answered out of the piece most likely to have room, rather than
        // failing against a chain of small ones.
        while remaining > 0 {
            let piece = prev_power_of_two(remaining);
            let base = PhysAddr(start.0 + offset);
            let tree = BuddyFrameAllocator::new(base, piece * PAGE_SIZE)?;
            self.trees.try_reserve(1).map_err(|_| KError::OutOfResources)?;
            self.trees.push(tree);
            offset += piece * PAGE_SIZE;
            remaining -= piece;
            added += piece;
        }
        Ok(added)
    }

    /// How many runs the pool is made of. One more than the number of times its
    /// memory was not a power of two.
    #[must_use]
    pub fn trees(&self) -> usize {
        self.trees.len()
    }

    /// Total frames under management.
    #[must_use]
    pub fn frames(&self) -> usize {
        self.trees.iter().map(BuddyFrameAllocator::frames).sum()
    }

    /// The largest run of contiguous free frames **any one tree** can offer.
    ///
    /// Not the total free memory, and not comparable across pool shapes: a pool
    /// of one 1024-frame tree and a pool of eight 128-frame trees both have 1024
    /// free frames, and this returns 1024 for the first and 128 for the second.
    /// That is the honest number, because it is the largest allocation either
    /// could actually satisfy.
    #[must_use]
    pub fn largest_free_run(&self) -> usize {
        self.trees.iter().map(BuddyFrameAllocator::largest_free_run).max().unwrap_or(0)
    }

    /// Sum over trees of each tree's largest free run.
    ///
    /// The invariant a self-test wants. It equals [`FramePool::frames`] exactly
    /// when every tree is entirely free **and** fully coalesced, so one leaked
    /// frame anywhere — in any tree — drops it. Comparing it before and after a
    /// round of allocation checks reclamation across the whole pool without the
    /// allocator tracking owners.
    #[must_use]
    pub fn coalesced_frames(&self) -> usize {
        self.trees.iter().map(BuddyFrameAllocator::largest_free_run).sum()
    }

    /// Allocate `count` contiguous frames, from a single tree.
    ///
    /// First fit across the trees. Not best fit: the trees are in descending size
    /// order within each run, so the first that fits is usually the tightest one
    /// that can, and walking all of them to find a marginally better answer costs
    /// more than it saves on a pool of a few dozen.
    pub fn alloc_pages(&mut self, count: usize) -> Option<PhysAddr> {
        self.trees.iter_mut().find_map(|t| t.alloc_pages(count))
    }

    /// Return a block to whichever tree owns it.
    ///
    /// A block from a foreign address belongs to no tree and is dropped rather
    /// than corrupting one: [`BuddyFrameAllocator::free_pages`] already ignores
    /// an address outside its own range, so the search is what makes this safe
    /// rather than a bounds check here.
    pub fn free_pages(&mut self, frame: PhysAddr) {
        for tree in &mut self.trees {
            if tree.contains(frame) {
                tree.free_pages(frame);
                return;
            }
        }
    }
}

impl Default for FramePool {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameAllocator for FramePool {
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
        1usize << (usize::BITS - 1 - n.leading_zeros())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1024 * 1024;

    #[test]
    fn nothing_is_rounded_away() {
        // The whole reason this type exists. 1956 MiB is what a 4 GiB QEMU guest
        // offers as its largest free run, and a single buddy tree manages 1024 of
        // it — 48% discarded with no message.
        let mut pool = FramePool::new();
        let frames = 1956 * MIB / PAGE_SIZE;
        let added = pool.add(PhysAddr(0x10_0000), 1956 * MIB).unwrap();
        assert_eq!(added, frames);
        assert_eq!(pool.frames(), frames);
        assert_eq!(
            BuddyFrameAllocator::metadata_bytes(1956 * MIB) / size_of::<u32>() / 2 + 1,
            1024 * MIB / PAGE_SIZE,
            "the single-tree allocator really does round down to 1024 MiB",
        );
    }

    #[test]
    fn the_decomposition_is_the_binary_expansion() {
        // 1956 = 1024 + 512 + 256 + 128 + 32 + 4, which is six set bits.
        let mut pool = FramePool::new();
        pool.add(PhysAddr(0), 1956 * PAGE_SIZE).unwrap();
        assert_eq!(pool.trees(), 1956usize.count_ones() as usize);
        assert_eq!(pool.frames(), 1956);
        // A power of two needs exactly one tree, which is the old behaviour and
        // must not have got more expensive.
        let mut pool = FramePool::new();
        pool.add(PhysAddr(0), 1024 * PAGE_SIZE).unwrap();
        assert_eq!(pool.trees(), 1);
    }

    #[test]
    fn pieces_are_laid_out_largest_first_and_do_not_overlap() {
        let mut pool = FramePool::new();
        pool.add(PhysAddr(0x20_0000), 7 * PAGE_SIZE).unwrap();
        // 7 = 4 + 2 + 1, in that order, contiguous from the base.
        assert_eq!(pool.trees(), 3);
        // Every frame in the run must be reachable, exactly once.
        let mut seen = alloc::vec::Vec::new();
        while let Some(f) = pool.allocate() {
            seen.push(f.0);
        }
        seen.sort_unstable();
        let expected: alloc::vec::Vec<usize> =
            (0..7).map(|i| 0x20_0000 + i * PAGE_SIZE).collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn several_runs_are_all_managed() {
        // A 4 GiB PC: RAM below the PCI hole, and the rest above 4 GiB. Taking
        // only the largest lost the other one entirely.
        let mut pool = FramePool::new();
        pool.add(PhysAddr(0x10_0000), 2048 * MIB).unwrap();
        pool.add(PhysAddr(0x1_0000_0000), 1994 * MIB).unwrap();
        assert_eq!(pool.frames(), (2048 + 1994) * MIB / PAGE_SIZE);

        // And an allocation can come out of either.
        let low = pool.alloc_pages(256).unwrap();
        assert!(low.0 < 0x1_0000_0000);
        // Exhaust the low run, then check the high one still answers.
        let mut taken = alloc::vec::Vec::new();
        while let Some(f) = pool.alloc_pages(1024) {
            taken.push(f);
        }
        assert!(taken.iter().any(|f| f.0 >= 0x1_0000_0000), "the high run was never used");
    }

    #[test]
    fn a_frame_goes_back_to_the_tree_that_owns_it() {
        let mut pool = FramePool::new();
        pool.add(PhysAddr(0), 4 * PAGE_SIZE).unwrap();
        pool.add(PhysAddr(0x1000_0000), 4 * PAGE_SIZE).unwrap();
        let before = pool.coalesced_frames();

        let a = pool.allocate().unwrap();
        let b = pool.alloc_pages(4).unwrap();
        assert_ne!(a.0 & !0xFFF_FFFF, b.0 & !0xFFF_FFFF, "the two came from different runs");
        pool.free_pages(a);
        pool.free_pages(b);
        assert_eq!(pool.coalesced_frames(), before, "a frame went back to the wrong tree");
    }

    #[test]
    fn a_foreign_address_is_ignored_not_absorbed() {
        // Freeing something the pool never owned must not make it believe it has
        // memory it does not, which would hand the same frame to two callers.
        let mut pool = FramePool::new();
        pool.add(PhysAddr(0x4000_0000), 8 * PAGE_SIZE).unwrap();
        let before = pool.coalesced_frames();
        pool.free_pages(PhysAddr(0x9000_0000));
        pool.free_pages(PhysAddr(0));
        assert_eq!(pool.coalesced_frames(), before);
        assert_eq!(pool.frames(), 8);
    }

    #[test]
    fn coalesced_frames_catches_a_leak_in_any_tree() {
        let mut pool = FramePool::new();
        pool.add(PhysAddr(0), 12 * PAGE_SIZE).unwrap(); // 8 + 4, two trees
        assert_eq!(pool.trees(), 2);
        assert_eq!(pool.coalesced_frames(), 12);

        // Take everything, give back all but one, from the *smaller* tree — the
        // one a check that only looked at the largest run would miss.
        let mut all = alloc::vec::Vec::new();
        while let Some(f) = pool.allocate() {
            all.push(f);
        }
        assert_eq!(all.len(), 12);
        for f in &all[1..] {
            pool.free_pages(*f);
        }
        assert!(pool.coalesced_frames() < 12, "one leaked frame went unnoticed");

        pool.free_pages(all[0]);
        assert_eq!(pool.coalesced_frames(), 12, "the pool did not fully coalesce");
    }

    #[test]
    fn the_largest_run_is_a_single_tree_not_the_total() {
        // Stated rather than hidden: this pool has 12 free frames and cannot
        // satisfy a request for 12, because they are not contiguous.
        let mut pool = FramePool::new();
        pool.add(PhysAddr(0), 12 * PAGE_SIZE).unwrap();
        assert_eq!(pool.coalesced_frames(), 12);
        assert_eq!(pool.largest_free_run(), 8);
        assert_eq!(pool.alloc_pages(12), None);
        assert!(pool.alloc_pages(8).is_some());
    }

    #[test]
    fn the_metadata_bill_is_exact() {
        // Two trees, 8 and 4 leaves: (2*8-1) + (2*4-1) = 22 nodes.
        assert_eq!(FramePool::metadata_bytes(12 * PAGE_SIZE), 22 * size_of::<u32>());
        // One tree, unchanged from the single-allocator cost.
        assert_eq!(
            FramePool::metadata_bytes(1024 * PAGE_SIZE),
            BuddyFrameAllocator::metadata_bytes(1024 * PAGE_SIZE),
        );
        // And it is the sum over runs, which is what the caller sizes the heap
        // from.
        let runs = [(0usize, 12 * PAGE_SIZE), (0x1000_0000, 6 * PAGE_SIZE)];
        assert_eq!(
            FramePool::metadata_bytes_for(&runs),
            FramePool::metadata_bytes(12 * PAGE_SIZE) + FramePool::metadata_bytes(6 * PAGE_SIZE),
        );
        // Nothing at all for a run with no whole frame.
        assert_eq!(FramePool::metadata_bytes(0), 0);
        assert_eq!(FramePool::metadata_bytes(PAGE_SIZE - 1), 0);
    }

    #[test]
    fn the_bill_is_the_price_of_managing_four_times_as_much() {
        // The trade this type makes, as a number. Roughly 4x the metadata for
        // roughly 4x the memory under management, i.e. the same 0.2% — worth
        // asserting so a future change to the decomposition cannot quietly make
        // it 10x.
        let len = 1956 * MIB;
        let old = BuddyFrameAllocator::metadata_bytes(len);
        let new = FramePool::metadata_bytes(len);
        assert!(new > old, "the new bill should be larger");
        assert!(new < 4 * old, "but not more than four times: {new} vs {old}");
        // 0.2% of the memory managed, as the doc claims.
        assert!(new * 500 < len, "metadata is more than 0.2% of the run");
    }

    #[test]
    fn an_unaligned_base_is_refused() {
        let mut pool = FramePool::new();
        assert_eq!(pool.add(PhysAddr(1), PAGE_SIZE).unwrap_err(), KError::InvalidArgument);
        assert_eq!(pool.trees(), 0);
    }

    #[test]
    fn a_run_with_no_whole_frame_adds_nothing() {
        let mut pool = FramePool::new();
        assert_eq!(pool.add(PhysAddr(0x1000), PAGE_SIZE - 1).unwrap(), 0);
        assert_eq!(pool.trees(), 0);
        assert_eq!(pool.allocate(), None);
    }
}
