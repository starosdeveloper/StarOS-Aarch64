//! Layer 2's other half: `mmap` and its relatives over `MapAnon`.
//!
//! `malloc` has been here since the first C program ran; `mmap` is what a program
//! calls when it wants pages rather than bytes — Qt maps font files and its own
//! resource blobs, and every allocator larger than ours asks for memory this way.
//!
//! ## What this layer can and cannot do, exactly
//! The kernel has two memory syscalls for user space: `MapAnon`, which adds pages,
//! and `Unmap`, which takes them away and returns the frames to the pool. There is
//! no protection change.
//!
//! * **`mmap`** of anonymous memory is real.
//! * **`mmap` of a file** is refused with `ENODEV`. Faulting a file in on demand
//!   needs the kernel to know that a page belongs to `fssrv`, which is a phase of
//!   its own; a version that read the whole file into anonymous memory would work
//!   until something wrote to a `MAP_SHARED` mapping and expected the file to
//!   change.
//! * **`munmap`** is real: the pages go, and the frames behind them go back to the
//!   allocator. It was not, for most of this library's life — it succeeded and left
//!   everything mapped — and rather than hide that,
//!   [`exports::staros_mmap_retained`] counted the bytes it had failed to release.
//!   The counter stays, now reading zero on the ordinary path and rising only when
//!   the kernel refuses a range or the range was already gone. A number that has
//!   always been the honest measure of a hole is worth keeping the day the hole is
//!   filled: it is what says so.
//!
//!   What is *not* returned is address space. The kernel's heap cursor moves forward
//!   only, so a freed address is never handed out twice, and a program that maps and
//!   unmaps forever walks to the end of its region eventually — 47 bits of it,
//!   against frames of megabytes.
//! * **`mprotect`** succeeds for anything that does not ask for `PROT_EXEC`, and
//!   refuses that with `EPERM`. The refusal is deliberate and load-bearing: the
//!   loader gives EL0 no way to make a page executable at run time, so a JIT — QML's
//!   included — must be told *no* at the call rather than discovering it as an
//!   instruction fetch from a non-executable page.
//! * **`madvise`** returns success and does nothing, which is what advice is.

use core::ffi::{c_int, c_void};

/// The failure value `mmap` returns, which is `-1` and not null.
pub(crate) const MAP_FAILED: *mut c_void = usize::MAX as *mut c_void;

pub(crate) const PROT_EXEC: c_int = 4;
pub(crate) const MAP_ANONYMOUS: c_int = 0x20;

/// Is this a request for plain anonymous memory — the only kind that can be served?
pub(crate) fn is_anonymous(flags: c_int, fd: c_int) -> bool {
    flags & MAP_ANONYMOUS != 0 || fd < 0
}

/// Round a byte count up to whole pages.
pub(crate) fn pages_for(bytes: usize) -> usize {
    bytes.div_ceil(4096)
}

/// The C entry points.
#[cfg(not(test))]
pub mod exports {
    use super::{c_int, c_void, is_anonymous, pages_for, MAP_FAILED, PROT_EXEC};
    use crate::{fail, sys};

    const ENOMEM: c_int = 12;
    const EINVAL: c_int = 22;
    const EPERM: c_int = 1;
    const ENODEV: c_int = 19;

    /// Bytes that were handed to `munmap` or `mremap` and are still mapped.
    ///
    /// Zero on the ordinary path now that `Unmap` exists. It rises when the kernel
    /// refuses a range, and when a caller frees something that was already gone —
    /// the second leaks nothing, and the two cannot be told apart from here, so the
    /// count reads high rather than low. A leak counter that guesses in the
    /// optimistic direction is a leak counter nobody can use.
    static mut RETAINED: usize = 0;

    /// # Safety
    /// C ABI. The returned pages are readable and writable and belong to the caller.
    #[no_mangle]
    pub unsafe extern "C" fn mmap(
        addr: *mut c_void,
        length: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void {
        if length == 0 {
            return fail(EINVAL, MAP_FAILED);
        }
        if !is_anonymous(flags, fd) {
            // A file mapping. Refused rather than faked — see the module comment.
            return fail(ENODEV, MAP_FAILED);
        }
        if prot & PROT_EXEC != 0 {
            return fail(EPERM, MAP_FAILED);
        }
        if !addr.is_null() {
            // A fixed address. The kernel picks addresses; honouring a hint it
            // cannot honour would silently place the mapping elsewhere, and
            // MAP_FIXED callers overwrite whatever is there on the strength of the
            // address they asked for.
            return fail(EINVAL, MAP_FAILED);
        }
        let _ = offset;
        match sys::map_anon(pages_for(length)) {
            Some(p) => p.cast::<c_void>(),
            None => fail(ENOMEM, MAP_FAILED),
        }
    }

    /// The 64-bit-offset name. On a 64-bit target it is the same function; glibc's
    /// headers pick between them, and Qt's objects reference this one.
    ///
    /// # Safety
    /// As [`mmap`].
    #[no_mangle]
    pub unsafe extern "C" fn mmap64(
        addr: *mut c_void,
        length: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void {
        // SAFETY: forwarded from the caller.
        unsafe { mmap(addr, length, prot, flags, fd, offset) }
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn munmap(addr: *mut c_void, length: usize) -> c_int {
        if addr.is_null() || length == 0 {
            return fail(EINVAL, -1);
        }
        let pages = pages_for(length);
        match sys::unmap(addr.cast::<u8>(), pages) {
            // The kernel returns how many pages it actually removed. Anything it did
            // not remove is either a page that was already gone — which leaks
            // nothing — or one it refused to touch, and only the second is worth
            // counting. They cannot be told apart from here, so the count is kept as
            // the conservative reading: pages asked about, minus pages removed.
            //
            // In the ordinary case that is zero, and `staros_mmap_retained()` reads
            // zero for the first time in this library's life.
            Some(removed) => {
                // SAFETY: a single word, under the same single-threaded assumption
                // as `errno`; it is a diagnostic, not a decision.
                unsafe { RETAINED += pages.saturating_sub(removed) * 4096 };
                0
            }
            // A refusal is not a reason to lie about it. The pages are still there,
            // the counter says so, and `munmap` returning success on a range the
            // kernel would not take is how a leak becomes invisible again.
            None => {
                // SAFETY: as above.
                unsafe { RETAINED += pages * 4096 };
                fail(EINVAL, -1)
            }
        }
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn mprotect(_addr: *mut c_void, _length: usize, prot: c_int) -> c_int {
        if prot & PROT_EXEC != 0 {
            // The one refusal that is not a limitation but a rule: nothing in EL0
            // gets an executable page after load time.
            return fail(EPERM, -1);
        }
        0
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn madvise(_addr: *mut c_void, _length: usize, _advice: c_int) -> c_int {
        0
    }

    /// `mremap`, which is real because it can be: a new mapping and a copy.
    ///
    /// It requires `MREMAP_MAYMOVE`, because without an unmap there is no way to
    /// grow a mapping in place and no way to promise the address is unchanged.
    ///
    /// # Safety
    /// C ABI: `old` is a mapping of `old_len` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn mremap(
        old: *mut c_void,
        old_len: usize,
        new_len: usize,
        flags: c_int,
        _extra: *mut c_void,
    ) -> *mut c_void {
        const MREMAP_MAYMOVE: c_int = 1;
        if flags & MREMAP_MAYMOVE == 0 {
            return fail(EINVAL, MAP_FAILED);
        }
        if old.is_null() || old_len == 0 || new_len == 0 {
            return fail(EINVAL, MAP_FAILED);
        }
        let Some(p) = sys::map_anon(pages_for(new_len)) else {
            return fail(ENOMEM, MAP_FAILED);
        };
        let copy = old_len.min(new_len);
        // The old mapping is given back after the copy, not before it — the bytes
        // are still being read out of it.
        // SAFETY: `old` is a mapping of `old_len` bytes by the caller's contract, and
        // `p` is a fresh mapping of at least `new_len`; the two cannot overlap.
        unsafe { core::ptr::copy_nonoverlapping(old.cast::<u8>(), p, copy) };
        let old_pages = pages_for(old_len);
        match sys::unmap(old.cast::<u8>(), old_pages) {
            // SAFETY: as in `munmap`.
            Some(removed) => unsafe { RETAINED += old_pages.saturating_sub(removed) * 4096 },
            // SAFETY: as above. The new mapping is still good, so this returns it
            // and records what the old one cost rather than failing a call that
            // succeeded.
            None => unsafe { RETAINED += old_pages * 4096 },
        }
        p.cast::<c_void>()
    }

    /// Bytes that `munmap` and `mremap` accounted for and the kernel still has
    /// mapped. Zero means nothing has been leaked yet, not that nothing can be.
    #[no_mangle]
    pub extern "C" fn staros_mmap_retained() -> usize {
        // SAFETY: reading a single word.
        unsafe { RETAINED }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_is_recognised_the_way_callers_spell_it() {
        // The two spellings that mean "no file": the flag, and a negative fd.
        assert!(is_anonymous(MAP_ANONYMOUS, -1));
        assert!(is_anonymous(0, -1));
        assert!(is_anonymous(MAP_ANONYMOUS, 3), "the flag wins over a stray fd");
        // A real file mapping, which this layer refuses rather than fakes.
        assert!(!is_anonymous(1, 3));
    }

    #[test]
    fn a_partial_page_still_costs_a_page() {
        assert_eq!(pages_for(1), 1);
        assert_eq!(pages_for(4096), 1);
        assert_eq!(pages_for(4097), 2);
        assert_eq!(pages_for(0), 0);
    }
}
