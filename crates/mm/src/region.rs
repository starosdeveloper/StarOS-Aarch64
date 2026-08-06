//! Physical address ranges, and carving the free ones out of a memory map.
//!
//! A machine's device tree says "RAM is here"; it does not say "and you may use
//! all of it". Sitting inside that RAM are the kernel's own image, the device
//! tree blob itself, an initrd the bootloader placed, and — on a real device —
//! a long list of firmware carveouts (trusted execution, modem, framebuffer).
//! Handing any of those to the frame allocator means overwriting them, and on a
//! phone that is an instant silent reset with no console to tell you why.
//!
//! So this module answers one question: *given a bank and a set of things that
//! are in the way, what is the largest run of memory I may actually use?* It is
//! pure arithmetic over `u64`s — no MMIO, no allocation (it runs before the heap
//! exists), and therefore fully host-testable, which is the only reason to trust
//! it before the first boot on hardware.

/// A half-open physical range `[start, end)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Region {
    /// First address in the range.
    pub start: u64,
    /// One past the last address in the range.
    pub end: u64,
}

impl Region {
    /// A range from `start` covering `len` bytes. Saturates rather than wrapping
    /// on overflow: the input comes from firmware and is not trusted.
    #[must_use]
    pub const fn new(start: u64, len: u64) -> Self {
        Self { start, end: start.saturating_add(len) }
    }

    /// A range spanning `[start, end)`. Empty if `end <= start`.
    #[must_use]
    pub const fn from_bounds(start: u64, end: u64) -> Self {
        Self { start, end }
    }

    /// Length in bytes; `0` for an empty or inverted range.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// Does the range cover no bytes?
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    /// Do the two ranges share at least one byte?
    #[must_use]
    pub const fn overlaps(&self, other: &Self) -> bool {
        self.start < other.end && other.start < self.end
    }

    /// Shrink to page boundaries: the start rounds *up* and the end rounds
    /// *down*, so the result is always a subset — never a byte more than was
    /// offered. Returns an empty region if nothing whole is left.
    #[must_use]
    pub const fn page_align_inward(&self, page: u64) -> Self {
        let start = match self.start.checked_add(page - 1) {
            Some(v) => v & !(page - 1),
            None => return Self { start: 0, end: 0 },
        };
        let end = self.end & !(page - 1);
        if end <= start {
            Self { start: 0, end: 0 }
        } else {
            Self { start, end }
        }
    }

    /// Split off the first `len` bytes, returning `(head, tail)`, or `None` if
    /// the region is too small. Used to carve the heap out of the front of the
    /// usable window before the rest becomes frames.
    #[must_use]
    pub const fn split_at(&self, len: u64) -> Option<(Self, Self)> {
        if self.len() < len {
            return None;
        }
        let mid = self.start + len;
        Some((
            Self { start: self.start, end: mid },
            Self { start: mid, end: self.end },
        ))
    }
}

/// The largest contiguous part of `bank` that no region in `exclusions` touches.
///
/// Exclusions may overlap each other, sit outside the bank, or be empty; all are
/// tolerated, because they come from firmware and from our own linker symbols
/// and there is no reason to assume they are tidy. Returns `None` if the bank is
/// entirely consumed.
///
/// The sweep is O(bank_gaps × exclusions), which for the few dozen carveouts a
/// real device declares is nothing, and it needs no allocation or sorting — both
/// of which matter because this runs before the heap exists.
#[must_use]
pub fn largest_free(bank: Region, exclusions: &[Region]) -> Option<Region> {
    let mut best: Option<Region> = None;
    let mut cursor = bank.start;

    while cursor < bank.end {
        // Find the exclusion that blocks us soonest at or after `cursor`.
        let mut blocker: Option<Region> = None;
        for ex in exclusions {
            if ex.is_empty() || ex.end <= cursor || ex.start >= bank.end {
                continue;
            }
            let clipped = Region {
                start: ex.start.max(cursor),
                end: ex.end.max(cursor),
            };
            if blocker.is_none_or(|b| clipped.start < b.start) {
                blocker = Some(clipped);
            }
        }

        let (gap, next) = match blocker {
            // Nothing else in the way: the rest of the bank is free.
            None => (Region::from_bounds(cursor, bank.end), bank.end),
            // Free up to the blocker, then resume past it. Overlapping
            // exclusions are handled by re-scanning from the new cursor.
            Some(b) => (Region::from_bounds(cursor, b.start), b.end),
        };

        if !gap.is_empty() && best.is_none_or(|w| gap.len() > w.len()) {
            best = Some(gap);
        }
        cursor = next;
    }

    best.filter(|r| !r.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: u64 = 4096;

    #[test]
    fn free_run_when_nothing_is_in_the_way() {
        let bank = Region::new(0x4000_0000, 0x1000_0000);
        let free = largest_free(bank, &[]).unwrap();
        assert_eq!(free, bank);
    }

    #[test]
    fn kernel_image_at_the_front_leaves_the_tail() {
        // The usual QEMU virt shape: RAM at 0x40000000, kernel loaded 512 KiB in.
        let bank = Region::new(0x4000_0000, 0x1000_0000);
        let kernel = Region::from_bounds(0x4008_0000, 0x4080_0000);
        let free = largest_free(bank, &[kernel]).unwrap();
        assert_eq!(free, Region::from_bounds(0x4080_0000, 0x5000_0000));
    }

    #[test]
    fn picks_the_larger_side_of_a_hole() {
        let bank = Region::from_bounds(0, 1000);
        // A hole near the front: the tail is bigger and must win.
        let free = largest_free(bank, &[Region::from_bounds(100, 200)]).unwrap();
        assert_eq!(free, Region::from_bounds(200, 1000));

        // A hole near the end: now the head wins.
        let free = largest_free(bank, &[Region::from_bounds(800, 900)]).unwrap();
        assert_eq!(free, Region::from_bounds(0, 800));
    }

    #[test]
    fn tolerates_overlapping_unsorted_and_outside_exclusions() {
        let bank = Region::from_bounds(1000, 2000);
        let exclusions = [
            Region::from_bounds(1500, 1600), // later hole, listed first
            Region::from_bounds(0, 1100),    // straddles the bank start
            Region::from_bounds(1550, 1700), // overlaps the first hole
            Region::from_bounds(5000, 6000), // entirely outside
            Region::from_bounds(300, 300),   // empty
        ];
        // Free: [1100,1500) = 400 and [1700,2000) = 300. The first wins.
        let free = largest_free(bank, &exclusions).unwrap();
        assert_eq!(free, Region::from_bounds(1100, 1500));
    }

    #[test]
    fn fully_consumed_bank_yields_nothing() {
        let bank = Region::from_bounds(0, 100);
        assert!(largest_free(bank, &[Region::from_bounds(0, 100)]).is_none());
        assert!(largest_free(bank, &[Region::from_bounds(0, 50), Region::from_bounds(50, 100)]).is_none());
        // An exclusion covering more than the bank must not underflow.
        assert!(largest_free(bank, &[Region::from_bounds(0, u64::MAX)]).is_none());
        assert!(largest_free(Region::from_bounds(0, 0), &[]).is_none());
    }

    #[test]
    fn a_phone_shaped_carveout_list() {
        // Realistic shape: a big bank with firmware reservations scattered
        // through it, of the kind /reserved-memory carries on a real device.
        let bank = Region::new(0x8000_0000, 0x8000_0000); // 2 GiB
        let exclusions = [
            Region::new(0x8000_0000, 0x0010_0000), // trusted firmware, at the base
            Region::new(0x8010_0000, 0x0080_0000), // kernel image
            Region::new(0x9000_0000, 0x0400_0000), // modem
            Region::new(0xF000_0000, 0x1000_0000), // framebuffer, at the very top
        ];
        let free = largest_free(bank, &exclusions).unwrap();
        // The winner is between the modem and the framebuffer.
        assert_eq!(free, Region::from_bounds(0x9400_0000, 0xF000_0000));
        for ex in &exclusions {
            assert!(!free.overlaps(ex), "free window must touch no carveout");
        }
    }

    #[test]
    fn alignment_only_ever_shrinks() {
        let r = Region::from_bounds(0x1001, 0x3FFF).page_align_inward(PAGE);
        assert_eq!(r, Region::from_bounds(0x2000, 0x3000));
        assert!(r.start >= 0x1001 && r.end <= 0x3FFF);

        // Nothing whole survives.
        assert!(Region::from_bounds(0x1001, 0x1002).page_align_inward(PAGE).is_empty());
    }

    #[test]
    fn split_carves_the_heap_off_the_front() {
        let r = Region::new(0x4000_0000, 0x1000);
        let (head, tail) = r.split_at(0x400).unwrap();
        assert_eq!(head, Region::from_bounds(0x4000_0000, 0x4000_0400));
        assert_eq!(tail, Region::from_bounds(0x4000_0400, 0x4000_1000));
        assert!(r.split_at(0x2000).is_none(), "cannot split more than exists");
    }
}
