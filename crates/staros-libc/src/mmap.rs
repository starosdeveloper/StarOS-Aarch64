//! Layer 2's other half: `mmap` and its relatives over `MapAnon`.
//!
//! `malloc` has been here since the first C program ran; `mmap` is what a program
//! calls when it wants pages rather than bytes — Qt maps font files and its own
//! resource blobs, and every allocator larger than ours asks for memory this way.
//!
//! ## What this layer can and cannot do, exactly
//! The kernel has one memory syscall for user space, `MapAnon`, and it only ever
//! *adds* pages to an address space. There is no unmap and no protection change, so:
//!
//! * **`mmap`** of anonymous memory is real.
//! * **`mmap` of a file** is refused with `ENODEV`. Faulting a file in on demand
//!   needs the kernel to know that a page belongs to `fssrv`, which is a phase of
//!   its own; a version that read the whole file into anonymous memory would work
//!   until something wrote to a `MAP_SHARED` mapping and expected the file to
//!   change.
//! * **`munmap`** succeeds and the pages stay mapped. This is the one place in this
//!   library where the answer is not the whole truth, so it is measured rather than
//!   hidden: [`exports::staros_mmap_retained`] reports how many bytes have been
//!   "unmapped" and are still there. A program whose working set is bounded never
//!   notices; one that maps and unmaps in a loop will, and the counter is how it
//!   finds out.
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

    /// Bytes that were handed to `munmap` and are still mapped. See the module
    /// comment: this is the price of a kernel with no unmap, made visible.
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
        // SAFETY: a single word, incremented under the same single-threaded
        // assumption as `errno`; it is a diagnostic, not a decision.
        unsafe { RETAINED += pages_for(length) * 4096 };
        0
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
        // SAFETY: `old` is a mapping of `old_len` bytes by the caller's contract, and
        // `p` is a fresh mapping of at least `new_len`; the two cannot overlap.
        unsafe { core::ptr::copy_nonoverlapping(old.cast::<u8>(), p, copy) };
        // SAFETY: as in `munmap` — the old pages stay, and the count says so.
        unsafe { RETAINED += pages_for(old_len) * 4096 };
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
