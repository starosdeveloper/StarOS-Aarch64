//! `qsort` and `bsearch` — the two `<stdlib.h>` algorithms, over untyped memory.
//!
//! Both were declared by this sysroot and implemented by nobody, which is how G6
//! found them. Qt has its own containers and sorts and will not call either, but
//! libstdc++'s `<cstdlib>` says `using ::qsort;` and any C a port drags in might.
//!
//! ## Why heapsort and not quicksort
//!
//! The name says quicksort and the standard does not. Quicksort's worst case is
//! quadratic, and the input that provokes it — already sorted, or all equal — is the
//! input real programs actually have. Heapsort is `O(n log n)` on every input, needs
//! no recursion and therefore no stack bound, and this system's stacks grow on
//! demand into a guard page: a sort that recursed to depth *n* on sorted input would
//! not be slow here, it would be a fault.
//!
//! The cost is that it is not stable. Neither is `qsort` by specification, so
//! nothing may depend on it — and a caller that needs stability was going to be
//! wrong on glibc too.
//!
//! ## Swapping without knowing the type
//!
//! Elements are `size` bytes of unknown alignment, so they move a byte at a time.
//! Reading them as words would be faster and would fault on the first `size` that is
//! not a multiple of the word — a struct of three chars in an array, say, which is
//! exactly the sort of thing that gets sorted.

use core::ffi::{c_int, c_void};

/// The comparison function C hands us.
pub type Compare = unsafe extern "C" fn(*const c_void, *const c_void) -> c_int;

/// Swap two elements of `size` bytes.
///
/// # Safety
/// Both pointers are valid for `size` bytes and do not overlap unless equal.
unsafe fn swap(a: *mut u8, b: *mut u8, size: usize) {
    if core::ptr::eq(a, b) {
        return;
    }
    for i in 0..size {
        // SAFETY: both runs are `size` bytes by the caller's contract.
        unsafe {
            let t = *a.add(i);
            *a.add(i) = *b.add(i);
            *b.add(i) = t;
        }
    }
}

/// Sift the element at `root` down through the heap rooted at `base`.
///
/// # Safety
/// `base` is `count * size` bytes; `cmp` is a valid comparison over them.
unsafe fn sift_down(base: *mut u8, count: usize, size: usize, mut root: usize, cmp: Compare) {
    loop {
        let left = 2 * root + 1;
        if left >= count {
            return;
        }
        let mut largest = root;
        // SAFETY: `left` is in range, checked above.
        let greater = |i: usize, j: usize| unsafe {
            cmp(base.add(i * size).cast(), base.add(j * size).cast()) > 0
        };
        if greater(left, largest) {
            largest = left;
        }
        let right = left + 1;
        if right < count && greater(right, largest) {
            largest = right;
        }
        if largest == root {
            return;
        }
        // SAFETY: both indices are below `count`.
        unsafe { swap(base.add(root * size), base.add(largest * size), size) };
        root = largest;
    }
}

/// Heapsort `count` elements of `size` bytes at `base`.
///
/// # Safety
/// `base` points at `count * size` writable bytes; `cmp` compares two of them.
pub unsafe fn sort(base: *mut u8, count: usize, size: usize, cmp: Compare) {
    if count < 2 || size == 0 {
        return;
    }
    // Build the heap from the last parent down.
    for start in (0..count / 2).rev() {
        // SAFETY: forwarded from this function's contract.
        unsafe { sift_down(base, count, size, start, cmp) };
    }
    // Repeatedly move the largest to the end and shrink the heap.
    for end in (1..count).rev() {
        // SAFETY: as above; both indices are below `count`.
        unsafe {
            swap(base, base.add(end * size), size);
            sift_down(base, end, size, 0, cmp);
        }
    }
}

/// Binary search over a sorted array. Returns the index of a matching element.
///
/// Returns *an* index, not the first: C says "a pointer to a matching element",
/// and which one is unspecified when several compare equal. Promising the first
/// would be a guarantee callers would come to rely on and glibc does not give.
///
/// # Safety
/// `base` points at `count * size` readable bytes, sorted by `cmp`.
pub unsafe fn search(
    key: *const c_void,
    base: *const u8,
    count: usize,
    size: usize,
    cmp: Compare,
) -> Option<usize> {
    if size == 0 {
        return None;
    }
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
        // `lo + (hi - lo) / 2`, not `(lo + hi) / 2`: the sum overflows for arrays
        // near half the address space, and this one takes a byte count that a
        // caller can pass anything for.
        let mid = lo + (hi - lo) / 2;
        // SAFETY: `mid < count`, so the element is inside the array.
        let order = unsafe { cmp(key, base.add(mid * size).cast()) };
        match order {
            0 => return Some(mid),
            o if o < 0 => hi = mid,
            _ => lo = mid + 1,
        }
    }
    None
}

/// The C entry points.
#[cfg(not(test))]
pub mod exports {
    use super::{c_void, Compare};

    /// # Safety
    /// C ABI: `base` is `count * size` writable bytes and `cmp` compares two of them.
    #[no_mangle]
    pub unsafe extern "C" fn qsort(
        base: *mut c_void,
        count: usize,
        size: usize,
        cmp: Option<Compare>,
    ) {
        let Some(cmp) = cmp else { return };
        if base.is_null() {
            return;
        }
        // SAFETY: forwarded from the caller.
        unsafe { super::sort(base.cast::<u8>(), count, size, cmp) };
    }

    /// # Safety
    /// C ABI: `base` is `count * size` readable bytes, sorted by `cmp`.
    #[no_mangle]
    pub unsafe extern "C" fn bsearch(
        key: *const c_void,
        base: *const c_void,
        count: usize,
        size: usize,
        cmp: Option<Compare>,
    ) -> *mut c_void {
        let Some(cmp) = cmp else {
            return core::ptr::null_mut();
        };
        if base.is_null() || key.is_null() {
            return core::ptr::null_mut();
        }
        let base = base.cast::<u8>();
        // SAFETY: forwarded from the caller.
        match unsafe { super::search(key, base, count, size, cmp) } {
            // SAFETY: the index came from inside the array.
            Some(i) => unsafe { base.add(i * size) as *mut c_void },
            None => core::ptr::null_mut(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn cmp_i32(a: *const c_void, b: *const c_void) -> c_int {
        // SAFETY: the tests only ever pass `i32`s.
        let (a, b) = unsafe { (*a.cast::<i32>(), *b.cast::<i32>()) };
        a.cmp(&b) as c_int
    }

    fn sorted(mut v: Vec<i32>) -> Vec<i32> {
        let n = v.len();
        // SAFETY: the slice is `n` elements of `i32` and the comparison reads two.
        unsafe { sort(v.as_mut_ptr().cast::<u8>(), n, 4, cmp_i32) };
        v
    }

    #[test]
    fn it_sorts_the_inputs_that_break_quicksort() {
        // Already sorted, reversed, and all equal — the three shapes that make a
        // naive quicksort quadratic and, on a stack that grows into a guard page,
        // make it fault rather than merely crawl.
        let up: Vec<i32> = (0..500).collect();
        assert_eq!(sorted(up.clone()), up);
        let down: Vec<i32> = (0..500).rev().collect();
        assert_eq!(sorted(down), up);
        assert_eq!(sorted(vec![7; 300]), vec![7; 300]);
    }

    #[test]
    fn it_sorts_an_arbitrary_permutation() {
        // A deterministic shuffle: a multiplier coprime with the modulus visits
        // every value once, which is a permutation without needing a random source.
        let v: Vec<i32> = (0..401).map(|i: i32| (i * 137) % 401).collect();
        let want: Vec<i32> = (0..401).collect();
        assert_eq!(sorted(v), want);
    }

    #[test]
    fn the_degenerate_sizes_do_nothing_rather_than_something_wrong() {
        assert_eq!(sorted(vec![]), Vec::<i32>::new());
        assert_eq!(sorted(vec![42]), vec![42]);
        let mut one = [3i32, 1, 2];
        // A zero element size is a caller's arithmetic gone wrong; sorting "nothing"
        // repeatedly would spin, and reordering bytes would be worse.
        // SAFETY: the pointer is valid; the size is the thing under test.
        unsafe { sort(one.as_mut_ptr().cast::<u8>(), 3, 0, cmp_i32) };
        assert_eq!(one, [3, 1, 2]);
    }

    #[test]
    fn search_finds_what_is_there_and_refuses_what_is_not() {
        let v: Vec<i32> = (0..100).map(|i| i * 3).collect();
        for (i, &x) in v.iter().enumerate() {
            // SAFETY: `v` is sorted and the comparison reads two `i32`s.
            let found = unsafe {
                search(core::ptr::from_ref(&x).cast(), v.as_ptr().cast::<u8>(), v.len(), 4, cmp_i32)
            };
            assert_eq!(found, Some(i), "bsearch({x})");
        }
        for missing in [-1, 1, 2, 298, 1000] {
            // SAFETY: as above.
            let found = unsafe {
                search(
                    core::ptr::from_ref(&missing).cast(),
                    v.as_ptr().cast::<u8>(),
                    v.len(),
                    4,
                    cmp_i32,
                )
            };
            assert_eq!(found, None, "bsearch({missing}) found something");
        }
    }

    #[test]
    fn search_of_an_empty_array_is_not_a_crash() {
        let v: Vec<i32> = Vec::new();
        let key = 1i32;
        // SAFETY: a zero-length array; the pointer is never dereferenced.
        let found = unsafe {
            search(core::ptr::from_ref(&key).cast(), v.as_ptr().cast::<u8>(), 0, 4, cmp_i32)
        };
        assert_eq!(found, None);
    }
}
