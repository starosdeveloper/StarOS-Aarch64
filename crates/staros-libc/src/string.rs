//! Layer 1 of the contract: the functions that are pure computation.
//!
//! Every function here is two things — a `pub(crate)` Rust function that does the
//! work over slices and raw pointers, and a `#[no_mangle] extern "C"` wrapper in
//! [`exports`](crate::string::exports) that C sees. The split is not ceremony: the
//! exported names are `memcpy`, `strlen` and friends, and a host test binary that
//! defined those would collide with the system's own at link time. Splitting them
//! means the logic is tested on the host and the ABI is exported only for the
//! bare-metal build.
//!
//! Eighty-four of the contract's symbols are in this layer, and this file covers
//! the ones a C program cannot start without. The rest (`wcs*`, the `_chk`
//! variants, the math library) are named in `docs/LIBC-CONTRACT.md` with their
//! status; nothing here pretends they exist.

use core::ffi::{c_char, c_int};
#[cfg(not(test))]
use core::ffi::c_void;

/// `strlen`: bytes before the NUL.
///
/// # Safety
/// `s` must point at a NUL-terminated string.
pub(crate) unsafe fn strlen(s: *const c_char) -> usize {
    let mut n = 0;
    // SAFETY: the caller guarantees a NUL exists; we stop at it and never read past.
    while unsafe { *s.add(n) } != 0 {
        n += 1;
    }
    n
}

/// The bytes of a C string, not including the NUL.
///
/// # Safety
/// As [`strlen`].
pub(crate) unsafe fn as_bytes<'a>(s: *const c_char) -> &'a [u8] {
    // SAFETY: forwarded; the length is exactly the bytes before the NUL.
    unsafe { core::slice::from_raw_parts(s.cast::<u8>(), strlen(s)) }
}

/// Compare two byte runs the way C does: by *unsigned* char value, returning the
/// difference of the first pair that differs.
///
/// Signedness is the whole subtlety. `c_char` is signed on this target, and a
/// comparison that keeps the sign puts every byte above 0x7f *below* every ASCII
/// byte — so a UTF-8 path sorts before "a", `strcmp` on non-ASCII returns the wrong
/// sign, and every user of it (a sorted map of file names, say) misbehaves in a way
/// that looks like data corruption rather than a comparison bug.
pub(crate) fn compare(a: &[u8], b: &[u8]) -> c_int {
    let n = a.len().min(b.len());
    for i in 0..n {
        if a[i] != b[i] {
            return c_int::from(a[i]) - c_int::from(b[i]);
        }
    }
    0
}

/// `memcmp` over slices.
pub(crate) fn memcmp(a: &[u8], b: &[u8]) -> c_int {
    debug_assert_eq!(a.len(), b.len());
    compare(a, b)
}

/// `strcmp` over the two strings' bytes: compare content, then length.
pub(crate) fn strcmp(a: &[u8], b: &[u8]) -> c_int {
    let d = compare(a, b);
    if d != 0 {
        return d;
    }
    // One is a prefix of the other: the shorter one's NUL loses to whatever byte
    // follows in the longer.
    match a.len().cmp(&b.len()) {
        core::cmp::Ordering::Less => -c_int::from(b[a.len()]),
        core::cmp::Ordering::Greater => c_int::from(a[b.len()]),
        core::cmp::Ordering::Equal => 0,
    }
}

/// `strncmp`: like [`strcmp`] but never looks past `n` bytes of either string.
pub(crate) fn strncmp(a: &[u8], b: &[u8], n: usize) -> c_int {
    let a = &a[..a.len().min(n)];
    let b = &b[..b.len().min(n)];
    if a.len() == n && b.len() == n {
        return compare(a, b);
    }
    strcmp(a, b)
}

/// Index of the first occurrence of `needle` in `haystack`.
pub(crate) fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// Length of the initial run of bytes that are **not** in `reject` (`strcspn`).
pub(crate) fn complement_span(s: &[u8], reject: &[u8]) -> usize {
    s.iter().position(|b| reject.contains(b)).unwrap_or(s.len())
}

/// Length of the initial run of bytes that are all in `accept` (`strspn`).
pub(crate) fn span(s: &[u8], accept: &[u8]) -> usize {
    s.iter().position(|b| !accept.contains(b)).unwrap_or(s.len())
}

/// `strtol` without the locale: optional space, optional sign, an optional `0x`
/// prefix when the base allows it, then digits. Returns the value and how many
/// bytes were consumed, so the caller can set `endptr` honestly.
///
/// Saturates rather than wrapping. A C program that feeds this an over-long number
/// gets `LONG_MAX`, which is what glibc does; wrapping would turn a too-large
/// timeout into a small one, which is the kind of bug that only shows up in
/// production.
pub(crate) fn strtol(s: &[u8], base: c_int) -> (i64, usize) {
    let mut i = 0;
    while i < s.len() && (s[i] == b' ' || (0x09..=0x0d).contains(&s[i])) {
        i += 1;
    }
    let negative = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let mut base = base;
    if (base == 0 || base == 16)
        && s.get(i) == Some(&b'0')
        && matches!(s.get(i + 1), Some(b'x' | b'X'))
        && s.get(i + 2).is_some_and(|c| digit(*c, 16).is_some())
    {
        i += 2;
        base = 16;
    } else if base == 0 {
        base = if s.get(i) == Some(&b'0') { 8 } else { 10 };
    }

    let start = i;
    let mut value: i64 = 0;
    let mut saturated = false;
    while let Some(d) = s.get(i).and_then(|c| digit(*c, base)) {
        value = match value
            .checked_mul(i64::from(base))
            .and_then(|v| v.checked_add(i64::from(d)))
        {
            Some(v) => v,
            None => {
                saturated = true;
                i64::MAX
            }
        };
        i += 1;
    }
    if i == start {
        // No digits at all: C says the value is 0 and nothing was consumed, which is
        // how a caller tells "0" from "not a number".
        return (0, 0);
    }
    let value = if saturated {
        if negative {
            i64::MIN
        } else {
            i64::MAX
        }
    } else if negative {
        -value
    } else {
        value
    };
    (value, i)
}

/// `strtoul`: the same shape as [`strtol`], accumulating unsigned.
///
/// Not a wrapper around `strtol`, and the reason is the range between `i64::MAX`
/// and `u64::MAX`. Those values are representable and `strtol` saturates them all
/// to `i64::MAX` — and they are exactly what a caller reaching for `strtoul`
/// usually has: a 64-bit identifier, a hash, a bit mask written in hex.
///
/// A leading `-` is accepted and negates, as C requires. That is genuinely strange
/// and it is the standard's: `strtoul("-1")` is `ULONG_MAX`, not an error, and a
/// program relying on it exists somewhere.
pub(crate) fn strtoul(s: &[u8], base: c_int) -> (u64, usize) {
    let mut i = 0;
    while i < s.len() && (s[i] == b' ' || (0x09..=0x0d).contains(&s[i])) {
        i += 1;
    }
    let negative = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let mut base = base;
    if (base == 0 || base == 16)
        && s.get(i) == Some(&b'0')
        && matches!(s.get(i + 1), Some(b'x' | b'X'))
        && s.get(i + 2).is_some_and(|c| digit(*c, 16).is_some())
    {
        i += 2;
        base = 16;
    } else if base == 0 {
        base = if s.get(i) == Some(&b'0') { 8 } else { 10 };
    }

    let start = i;
    let mut value: u64 = 0;
    let mut saturated = false;
    while let Some(d) = s.get(i).and_then(|c| digit(*c, base)) {
        value = match value
            .checked_mul(base as u64)
            .and_then(|v| v.checked_add(u64::from(d)))
        {
            Some(v) => v,
            None => {
                saturated = true;
                u64::MAX
            }
        };
        i += 1;
    }
    if i == start {
        return (0, 0);
    }
    let value = if saturated {
        u64::MAX
    } else if negative {
        value.wrapping_neg()
    } else {
        value
    };
    (value, i)
}

/// One digit in `base`, or `None`.
fn digit(c: u8, base: c_int) -> Option<u8> {
    let v = match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'z' => c - b'a' + 10,
        b'A'..=b'Z' => c - b'A' + 10,
        _ => return None,
    };
    (c_int::from(v) < base).then_some(v)
}

/// The C-ABI surface. Compiled only for the real target: on the host these names
/// belong to the system's own libc and defining them would break the test binary.
#[cfg(not(test))]
pub mod exports {
    use super::{as_bytes, c_char, c_int, c_void};

    // The three memory primitives are written as explicit loops, and the crate
    // carries `#![no_builtins]` so LLVM leaves them alone.
    //
    // The obvious implementation — `core::ptr::copy_nonoverlapping` — compiles to a
    // call to `memcpy`, which in a crate that *defines* `memcpy` is a call to
    // itself. It recurses until the stack guard stops it, and the symptom is a
    // program that prints its first line and then dies somewhere unrelated. (This
    // exact bug is what the G5.0 backtrace work paid for: `bt` named `memcpy`
    // called from `strcpy` at the first line of C that copies anything.)

    /// # Safety
    /// C ABI: `dst` and `src` must be valid for `n` bytes and not overlap.
    #[no_mangle]
    pub unsafe extern "C" fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
        let (d, s) = (dst.cast::<u8>(), src.cast::<u8>());
        let mut i = 0;
        // Word at a time while both sides are 8-aligned: the copies that matter here
        // are page-sized buffers moving through the file layer.
        if d as usize % 8 == 0 && s as usize % 8 == 0 {
            while i + 8 <= n {
                // SAFETY: `i + 8 <= n` and both pointers are valid for `n` bytes.
                unsafe { d.add(i).cast::<u64>().write(s.add(i).cast::<u64>().read()) };
                i += 8;
            }
        }
        while i < n {
            // SAFETY: as above, one byte at a time.
            unsafe { *d.add(i) = *s.add(i) };
            i += 1;
        }
        dst
    }

    /// # Safety
    /// C ABI: both pointers valid for `n` bytes; overlap is allowed.
    #[no_mangle]
    pub unsafe extern "C" fn memmove(
        dst: *mut c_void,
        src: *const c_void,
        n: usize,
    ) -> *mut c_void {
        let (d, s) = (dst.cast::<u8>(), src.cast::<u8>());
        if (d as usize) < s as usize {
            let mut i = 0;
            while i < n {
                // SAFETY: forwarded from the caller.
                unsafe { *d.add(i) = *s.add(i) };
                i += 1;
            }
        } else {
            // Backwards, so an overlapping move does not overwrite what it has yet
            // to read. This direction is the whole difference from `memcpy`.
            let mut i = n;
            while i > 0 {
                i -= 1;
                // SAFETY: forwarded from the caller.
                unsafe { *d.add(i) = *s.add(i) };
            }
        }
        dst
    }

    /// # Safety
    /// C ABI: `dst` valid for `n` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn memset(dst: *mut c_void, byte: c_int, n: usize) -> *mut c_void {
        let d = dst.cast::<u8>();
        let b = byte as u8;
        let mut i = 0;
        if d as usize % 8 == 0 {
            let word = u64::from_ne_bytes([b; 8]);
            while i + 8 <= n {
                // SAFETY: `i + 8 <= n` and `dst` is valid for `n` bytes.
                unsafe { d.add(i).cast::<u64>().write(word) };
                i += 8;
            }
        }
        while i < n {
            // SAFETY: as above.
            unsafe { *d.add(i) = b };
            i += 1;
        }
        dst
    }

    /// # Safety
    /// C ABI: both pointers valid for `n` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> c_int {
        // SAFETY: forwarded from the caller.
        let (a, b) = unsafe {
            (
                core::slice::from_raw_parts(a.cast::<u8>(), n),
                core::slice::from_raw_parts(b.cast::<u8>(), n),
            )
        };
        super::memcmp(a, b)
    }

    /// # Safety
    /// C ABI: `s` valid for `n` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn memchr(s: *const c_void, byte: c_int, n: usize) -> *mut c_void {
        // SAFETY: forwarded from the caller.
        let bytes = unsafe { core::slice::from_raw_parts(s.cast::<u8>(), n) };
        match bytes.iter().position(|&b| b == byte as u8) {
            // SAFETY: the index came from this very slice.
            Some(i) => unsafe { s.cast::<u8>().add(i) as *mut c_void },
            None => core::ptr::null_mut(),
        }
    }

    /// # Safety
    /// C ABI: NUL-terminated string.
    #[no_mangle]
    pub unsafe extern "C" fn strlen(s: *const c_char) -> usize {
        // SAFETY: forwarded from the caller.
        unsafe { super::strlen(s) }
    }

    /// # Safety
    /// C ABI: both NUL-terminated.
    #[no_mangle]
    pub unsafe extern "C" fn strcmp(a: *const c_char, b: *const c_char) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { super::strcmp(as_bytes(a), as_bytes(b)) }
    }

    /// # Safety
    /// C ABI: both NUL-terminated.
    #[no_mangle]
    pub unsafe extern "C" fn strncmp(a: *const c_char, b: *const c_char, n: usize) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { super::strncmp(as_bytes(a), as_bytes(b), n) }
    }

    /// # Safety
    /// C ABI: `src` NUL-terminated, `dst` big enough for it and the NUL.
    #[no_mangle]
    pub unsafe extern "C" fn strcpy(dst: *mut c_char, src: *const c_char) -> *mut c_char {
        // SAFETY: forwarded from the caller.
        unsafe {
            let n = super::strlen(src);
            core::ptr::copy_nonoverlapping(src, dst, n + 1);
        }
        dst
    }

    /// # Safety
    /// C ABI: `src` NUL-terminated, `dst` valid for `n` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn strncpy(dst: *mut c_char, src: *const c_char, n: usize) -> *mut c_char {
        // SAFETY: forwarded from the caller. C's `strncpy` pads with NULs and does
        // *not* terminate when the source is longer — both are part of the contract
        // and both are what callers depend on.
        unsafe {
            let len = super::strlen(src).min(n);
            core::ptr::copy_nonoverlapping(src, dst, len);
            core::ptr::write_bytes(dst.add(len), 0, n - len);
        }
        dst
    }

    /// # Safety
    /// C ABI: both NUL-terminated, `dst` big enough for the concatenation.
    #[no_mangle]
    pub unsafe extern "C" fn strcat(dst: *mut c_char, src: *const c_char) -> *mut c_char {
        // SAFETY: forwarded from the caller.
        unsafe {
            let end = super::strlen(dst);
            strcpy(dst.add(end), src);
        }
        dst
    }

    /// # Safety
    /// C ABI: NUL-terminated string.
    #[no_mangle]
    pub unsafe extern "C" fn strchr(s: *const c_char, ch: c_int) -> *mut c_char {
        // SAFETY: forwarded from the caller; the NUL itself is searchable, as in C.
        unsafe {
            let n = super::strlen(s);
            for i in 0..=n {
                if *s.add(i) as u8 == ch as u8 {
                    return s.add(i) as *mut c_char;
                }
            }
        }
        core::ptr::null_mut()
    }

    /// # Safety
    /// C ABI: NUL-terminated string.
    #[no_mangle]
    pub unsafe extern "C" fn strrchr(s: *const c_char, ch: c_int) -> *mut c_char {
        // SAFETY: forwarded from the caller.
        unsafe {
            let n = super::strlen(s);
            for i in (0..=n).rev() {
                if *s.add(i) as u8 == ch as u8 {
                    return s.add(i) as *mut c_char;
                }
            }
        }
        core::ptr::null_mut()
    }

    /// # Safety
    /// C ABI: both NUL-terminated.
    #[no_mangle]
    pub unsafe extern "C" fn strstr(haystack: *const c_char, needle: *const c_char) -> *mut c_char {
        // SAFETY: forwarded from the caller.
        unsafe {
            match super::find(as_bytes(haystack), as_bytes(needle)) {
                Some(i) => haystack.add(i) as *mut c_char,
                None => core::ptr::null_mut(),
            }
        }
    }

    /// # Safety
    /// C ABI: both NUL-terminated.
    #[no_mangle]
    pub unsafe extern "C" fn strcspn(s: *const c_char, reject: *const c_char) -> usize {
        // SAFETY: forwarded from the caller.
        unsafe { super::complement_span(as_bytes(s), as_bytes(reject)) }
    }

    /// # Safety
    /// C ABI: both NUL-terminated.
    #[no_mangle]
    pub unsafe extern "C" fn strspn(s: *const c_char, accept: *const c_char) -> usize {
        // SAFETY: forwarded from the caller.
        unsafe { super::span(as_bytes(s), as_bytes(accept)) }
    }

    /// # Safety
    /// C ABI: `s` NUL-terminated; `end`, if non-null, receives a pointer into `s`.
    #[no_mangle]
    pub unsafe extern "C" fn strtol(s: *const c_char, end: *mut *mut c_char, base: c_int) -> i64 {
        // SAFETY: forwarded from the caller.
        unsafe {
            let bytes = as_bytes(s);
            let (value, used) = super::strtol(bytes, base);
            if !end.is_null() {
                *end = s.add(used) as *mut c_char;
            }
            value
        }
    }

    /// # Safety
    /// C ABI: NUL-terminated string.
    #[no_mangle]
    pub unsafe extern "C" fn atoi(s: *const c_char) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { super::strtol(as_bytes(s), 10).0 as c_int }
    }

    #[no_mangle]
    pub extern "C" fn abs(v: c_int) -> c_int {
        v.wrapping_abs()
    }

    #[no_mangle]
    pub extern "C" fn labs(v: i64) -> i64 {
        v.wrapping_abs()
    }

    /// The `<inttypes.h>` family: the same operations over `intmax_t`, which on
    /// this target is `long`. Separate symbols because the *type* is what a caller
    /// named, and the day `intmax_t` is not `long` these are the four that change.
    #[no_mangle]
    pub extern "C" fn imaxabs(v: i64) -> i64 {
        v.wrapping_abs()
    }

    #[no_mangle]
    pub extern "C" fn imaxdiv(numer: i64, denom: i64) -> LdivT {
        ldiv(numer, denom)
    }

    /// # Safety
    /// C ABI: as [`strtol`].
    #[no_mangle]
    pub unsafe extern "C" fn strtoimax(
        s: *const c_char,
        end: *mut *mut c_char,
        base: c_int,
    ) -> i64 {
        // SAFETY: forwarded from the caller.
        unsafe { strtol(s, end, base) }
    }

    /// The unsigned parse.
    ///
    /// The digits are accumulated as **unsigned**, which is the whole difference
    /// from `strtol` and not a detail: a value between `LONG_MAX` and `ULONG_MAX`
    /// is representable here and saturates in `strtol`. A `strtoul` that forwarded
    /// to `strtol` would turn every such number into `LONG_MAX` — and the numbers
    /// in that range are exactly the ones a program parses with `strtoul` on
    /// purpose, like a 64-bit hash written in hex.
    ///
    /// # Safety
    /// C ABI: `s` is a NUL-terminated string; `end`, if not null, receives the
    /// first byte not consumed.
    #[no_mangle]
    pub unsafe extern "C" fn strtoul(
        s: *const c_char,
        end: *mut *mut c_char,
        base: c_int,
    ) -> u64 {
        // SAFETY: forwarded from the caller.
        let bytes = unsafe { as_bytes(s) };
        let (value, used) = super::strtoul(bytes, base);
        if !end.is_null() {
            // SAFETY: the caller passes a writable pointer, as the prototype says.
            unsafe { *end = s.add(used) as *mut c_char };
        }
        value
    }

    /// # Safety
    /// C ABI: as [`strtoul`].
    #[no_mangle]
    pub unsafe extern "C" fn strtoumax(
        s: *const c_char,
        end: *mut *mut c_char,
        base: c_int,
    ) -> u64 {
        // SAFETY: forwarded from the caller.
        unsafe { strtoul(s, end, base) }
    }

    /// `strtoll` and `strtoull`, which are `strtol` and `strtoul` here: `long` and
    /// `long long` are both 64 bits on this target. Declared and defined separately
    /// because a caller wrote one name or the other, and a program compiled for a
    /// system where they differ must keep meaning what it said.
    ///
    /// # Safety
    /// C ABI: as [`strtol`].
    #[no_mangle]
    pub unsafe extern "C" fn strtoll(
        s: *const c_char,
        end: *mut *mut c_char,
        base: c_int,
    ) -> i64 {
        // SAFETY: forwarded from the caller.
        unsafe { strtol(s, end, base) }
    }

    /// # Safety
    /// C ABI: as [`strtol`].
    #[no_mangle]
    pub unsafe extern "C" fn strtoull(
        s: *const c_char,
        end: *mut *mut c_char,
        base: c_int,
    ) -> u64 {
        // SAFETY: forwarded from the caller.
        unsafe { strtoul(s, end, base) }
    }

    #[no_mangle]
    pub extern "C" fn llabs(v: i64) -> i64 {
        v.wrapping_abs()
    }

    /// `div_t` and friends: quotient and remainder together.
    ///
    /// The struct is returned by value, which is the whole reason these exist —
    /// a caller wanting both would otherwise divide twice and hope the compiler
    /// noticed. Layout is quotient first, as every ABI on this platform has it.
    #[repr(C)]
    pub struct DivT {
        pub quot: c_int,
        pub rem: c_int,
    }

    #[repr(C)]
    pub struct LdivT {
        pub quot: i64,
        pub rem: i64,
    }

    #[no_mangle]
    pub extern "C" fn div(numer: c_int, denom: c_int) -> DivT {
        DivT { quot: numer.wrapping_div(denom), rem: numer.wrapping_rem(denom) }
    }

    #[no_mangle]
    pub extern "C" fn ldiv(numer: i64, denom: i64) -> LdivT {
        LdivT { quot: numer.wrapping_div(denom), rem: numer.wrapping_rem(denom) }
    }

    #[no_mangle]
    pub extern "C" fn lldiv(numer: i64, denom: i64) -> LdivT {
        ldiv(numer, denom)
    }

    /// `strpbrk`: the first byte of `s` that appears in `accept`.
    ///
    /// # Safety
    /// C ABI: both are NUL-terminated strings.
    #[no_mangle]
    pub unsafe extern "C" fn strpbrk(s: *const c_char, accept: *const c_char) -> *mut c_char {
        // SAFETY: forwarded from the caller.
        let (haystack, set) = unsafe { (as_bytes(s), as_bytes(accept)) };
        match haystack.iter().position(|b| set.contains(b)) {
            // SAFETY: the index is inside the string.
            Some(i) => unsafe { s.add(i) as *mut c_char },
            None => core::ptr::null_mut(),
        }
    }

    /// `memccpy`: copy until `c` has been copied, or `n` bytes have.
    ///
    /// Returns the byte *after* the copied `c`, or null if it never appeared —
    /// which is the only way a caller can tell "found it" from "ran out of room".
    ///
    /// # Safety
    /// C ABI: `dst` and `src` are valid for `n` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn memccpy(
        dst: *mut c_void,
        src: *const c_void,
        c: c_int,
        n: usize,
    ) -> *mut c_void {
        let needle = c as u8;
        let (d, s) = (dst.cast::<u8>(), src.cast::<u8>());
        for i in 0..n {
            // SAFETY: `i < n` and both runs are `n` bytes by the caller's contract.
            unsafe {
                let b = *s.add(i);
                *d.add(i) = b;
                if b == needle {
                    return d.add(i + 1).cast::<c_void>();
                }
            }
        }
        core::ptr::null_mut()
    }

    /// `strdup`: a copy of `s` from `malloc`.
    ///
    /// # Safety
    /// C ABI: `s` is a NUL-terminated string. The caller owns the result and must
    /// `free` it.
    #[no_mangle]
    pub unsafe extern "C" fn strdup(s: *const c_char) -> *mut c_char {
        // SAFETY: forwarded from the caller.
        let bytes = unsafe { as_bytes(s) };
        // SAFETY: `malloc` returns either null or `len + 1` writable bytes.
        let out = unsafe { crate::exports::malloc(bytes.len() + 1) }.cast::<u8>();
        if out.is_null() {
            return core::ptr::null_mut();
        }
        // SAFETY: `out` has room for the bytes and the NUL.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), out, bytes.len());
            *out.add(bytes.len()) = 0;
        }
        out.cast::<c_char>()
    }

    /// `strndup`: at most `n` bytes of `s`, always NUL-terminated.
    ///
    /// # Safety
    /// As [`strdup`], except `s` need only be readable for `n` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn strndup(s: *const c_char, n: usize) -> *mut c_char {
        let mut len = 0;
        // SAFETY: the caller promises `n` readable bytes; we stop at the NUL or at n.
        while len < n && unsafe { *s.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `malloc` returns either null or `len + 1` writable bytes.
        let out = unsafe { crate::exports::malloc(len + 1) }.cast::<u8>();
        if out.is_null() {
            return core::ptr::null_mut();
        }
        // SAFETY: `len` bytes are readable and `out` has room for them and the NUL.
        unsafe {
            core::ptr::copy_nonoverlapping(s.cast::<u8>(), out, len);
            *out.add(len) = 0;
        }
        out.cast::<c_char>()
    }

    /// Where `strtok` left off. One word for the process, which is what makes
    /// `strtok` the function every style guide tells you not to use: two callers
    /// interleaving calls corrupt each other's traversal, and `strtok_r` exists for
    /// exactly that reason. Implemented in terms of it so there is one algorithm.
    static mut STRTOK_SAVE: *mut c_char = core::ptr::null_mut();

    /// # Safety
    /// C ABI: `s` is a NUL-terminated string or null; `delim` is one.
    #[no_mangle]
    pub unsafe extern "C" fn strtok(s: *mut c_char, delim: *const c_char) -> *mut c_char {
        // SAFETY: forwarded; the saved pointer is this library's own.
        unsafe { strtok_r(s, delim, core::ptr::addr_of_mut!(STRTOK_SAVE)) }
    }

    /// `strxfrm`: transform for collation.
    ///
    /// In the "C" locale collation *is* byte order, so the transform is a copy and
    /// `strcoll` is `strcmp`. Saying that plainly beats a table that encodes the
    /// identity: the day a real locale exists, this is the function to change, and
    /// it should not look like it already does something.
    ///
    /// # Safety
    /// C ABI: `src` is a NUL-terminated string; `dst` is writable for `n` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn strxfrm(dst: *mut c_char, src: *const c_char, n: usize) -> usize {
        // SAFETY: forwarded from the caller.
        let bytes = unsafe { as_bytes(src) };
        if n > 0 {
            let copy = bytes.len().min(n - 1);
            // SAFETY: `copy < n` and `dst` is writable for `n`.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.cast::<u8>(), copy);
                *dst.add(copy) = 0;
            }
        }
        // C returns the length the transform *would* need, which may exceed `n` —
        // that is how a caller learns to allocate and try again.
        bytes.len()
    }

    // `isspace` and `isdigit` used to be here, alone, because `strtol` needed them
    // and nothing else did. They live in `crate::ctype` now with the other twelve —
    // a header that declared fourteen against an archive that defined two is the
    // defect that found the rest.

    /// The name glibc's headers give `strtol` when a program is compiled as C23.
    /// The C23 change is that base 2 gets a `0b` prefix; everything else, including
    /// this implementation, is the same function.
    ///
    /// # Safety
    /// As [`strtol`].
    #[no_mangle]
    pub unsafe extern "C" fn __isoc23_strtol(
        s: *const c_char,
        end: *mut *mut c_char,
        base: c_int,
    ) -> i64 {
        // SAFETY: forwarded from the caller.
        unsafe { strtol(s, end, base) }
    }

    /// # Safety
    /// C ABI: `dst` is NUL-terminated and has room for `n` more bytes and a NUL.
    #[no_mangle]
    pub unsafe extern "C" fn strncat(dst: *mut c_char, src: *const c_char, n: usize) -> *mut c_char {
        // SAFETY: forwarded from the caller.
        unsafe {
            let at = super::strlen(dst);
            let bytes = as_bytes(src);
            let take = bytes.len().min(n);
            for (i, &b) in bytes[..take].iter().enumerate() {
                *dst.add(at + i) = b as c_char;
            }
            // `strncat` always terminates — unlike `strncpy`, which is the source of
            // half the confusion between them.
            *dst.add(at + take) = 0;
        }
        dst
    }

    /// `strtok_r`: the reentrant one, and the only one worth having. The state lives
    /// in the caller's variable rather than in a static, so two threads tokenising
    /// two strings do not interleave.
    ///
    /// # Safety
    /// C ABI: `save` points at a pointer this function owns between calls.
    #[no_mangle]
    pub unsafe extern "C" fn strtok_r(
        s: *mut c_char,
        delim: *const c_char,
        save: *mut *mut c_char,
    ) -> *mut c_char {
        // SAFETY: forwarded from the caller.
        unsafe {
            let mut cur = if s.is_null() { *save } else { s };
            if cur.is_null() {
                return core::ptr::null_mut();
            }
            let delims = as_bytes(delim);
            // Skip leading delimiters; a run of them is one separator.
            while *cur != 0 && delims.contains(&(*cur as u8)) {
                cur = cur.add(1);
            }
            if *cur == 0 {
                *save = cur;
                return core::ptr::null_mut();
            }
            let start = cur;
            while *cur != 0 && !delims.contains(&(*cur as u8)) {
                cur = cur.add(1);
            }
            if *cur != 0 {
                // Cut the token out of the caller's buffer, which is what makes this
                // destructive and why a string literal must never be passed to it.
                *cur = 0;
                cur = cur.add(1);
            }
            *save = cur;
            start
        }
    }

    /// # Safety
    /// C ABI: both regions valid for their lengths.
    #[no_mangle]
    pub unsafe extern "C" fn memmem(
        haystack: *const c_void,
        haystack_len: usize,
        needle: *const c_void,
        needle_len: usize,
    ) -> *mut c_void {
        // SAFETY: forwarded from the caller.
        unsafe {
            let h = core::slice::from_raw_parts(haystack.cast::<u8>(), haystack_len);
            let n = core::slice::from_raw_parts(needle.cast::<u8>(), needle_len);
            match super::find(h, n) {
                Some(i) => haystack.cast::<u8>().add(i).cast_mut().cast::<c_void>(),
                None => core::ptr::null_mut(),
            }
        }
    }

    /// # Safety
    /// C ABI: `s` valid for `n` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn memrchr(s: *const c_void, byte: c_int, n: usize) -> *mut c_void {
        // SAFETY: forwarded from the caller.
        unsafe {
            let bytes = core::slice::from_raw_parts(s.cast::<u8>(), n);
            match bytes.iter().rposition(|&b| b == byte as u8) {
                Some(i) => s.cast::<u8>().add(i).cast_mut().cast::<c_void>(),
                None => core::ptr::null_mut(),
            }
        }
    }

    /// # Safety
    /// C ABI: NUL-terminated wide string.
    #[no_mangle]
    pub unsafe extern "C" fn wcslen(s: *const u32) -> usize {
        let mut n = 0;
        // SAFETY: forwarded from the caller; `wchar_t` is 32-bit on this target.
        while unsafe { *s.add(n) } != 0 {
            n += 1;
        }
        n
    }

    /// `strerror`. The strings are the real messages for the codes this library
    /// actually produces; anything else says so with its number rather than
    /// claiming to be "Unknown error", which tells the reader nothing.
    #[no_mangle]
    pub extern "C" fn strerror(code: c_int) -> *mut c_char {
        // Static storage, because C says the result stays valid until the next call.
        // Not thread-safe, and neither is glibc's — `strerror_r` exists for that.
        static mut UNKNOWN: [u8; 32] = [0; 32];
        let text: &[u8] = match code {
            0 => b"Success\0",
            1 => b"Operation not permitted\0",
            2 => b"No such file or directory\0",
            9 => b"Bad file descriptor\0",
            11 => b"Resource temporarily unavailable\0",
            12 => b"Cannot allocate memory\0",
            13 => b"Permission denied\0",
            14 => b"Bad address\0",
            16 => b"Device or resource busy\0",
            17 => b"File exists\0",
            21 => b"Is a directory\0",
            22 => b"Invalid argument\0",
            23 => b"Too many open files in system\0",
            24 => b"Too many open files\0",
            28 => b"No space left on device\0",
            32 => b"Broken pipe\0",
            38 => b"Function not implemented\0",
            110 => b"Connection timed out\0",
            _ => {
                // SAFETY: single-threaded use of a static, matching C's own rule
                // about the lifetime of this result.
                unsafe {
                    let buf = &mut *core::ptr::addr_of_mut!(UNKNOWN);
                    let prefix = b"Error ";
                    buf[..prefix.len()].copy_from_slice(prefix);
                    let mut n = prefix.len();
                    let mut digits = [0u8; 10];
                    let mut d = 0;
                    let mut v = code.unsigned_abs();
                    loop {
                        digits[d] = b'0' + (v % 10) as u8;
                        v /= 10;
                        d += 1;
                        if v == 0 {
                            break;
                        }
                    }
                    while d > 0 {
                        d -= 1;
                        buf[n] = digits[d];
                        n += 1;
                    }
                    buf[n] = 0;
                    return buf.as_mut_ptr().cast::<c_char>();
                }
            }
        };
        text.as_ptr().cast::<c_char>().cast_mut()
    }

    /// # Safety
    /// C ABI: `buf` valid for `len` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn strerror_r(code: c_int, buf: *mut c_char, len: usize) -> c_int {
        let src = strerror(code);
        // SAFETY: forwarded from the caller.
        unsafe {
            let bytes = as_bytes(src);
            if bytes.len() + 1 > len {
                return 34; // ERANGE
            }
            for (i, &b) in bytes.iter().enumerate() {
                *buf.add(i) = b as c_char;
            }
            *buf.add(bytes.len()) = 0;
        }
        0
    }

    // The fortified variants. `_FORTIFY_SOURCE` makes the compiler pass the
    // destination's size — which it often knows and the callee never does — so the
    // overflow can be caught before it happens rather than found afterwards as a
    // corrupted neighbour. Qt is built with it on, which is why these are in the
    // contract; each one checks and then defers to the function it guards.

    /// # Safety
    /// C ABI: as `memcpy`, plus `size` describing `dst`.
    #[no_mangle]
    pub unsafe extern "C" fn __memcpy_chk(
        dst: *mut c_void,
        src: *const c_void,
        n: usize,
        size: usize,
    ) -> *mut c_void {
        if n > size {
            crate::chk_fail("memcpy");
        }
        // SAFETY: checked above, then forwarded.
        unsafe { memcpy(dst, src, n) }
    }

    /// # Safety
    /// C ABI: as `memmove`, plus `size` describing `dst`.
    #[no_mangle]
    pub unsafe extern "C" fn __memmove_chk(
        dst: *mut c_void,
        src: *const c_void,
        n: usize,
        size: usize,
    ) -> *mut c_void {
        if n > size {
            crate::chk_fail("memmove");
        }
        // SAFETY: checked above, then forwarded.
        unsafe { memmove(dst, src, n) }
    }

    /// # Safety
    /// C ABI: as `memset`, plus `size` describing `dst`.
    #[no_mangle]
    pub unsafe extern "C" fn __memset_chk(
        dst: *mut c_void,
        byte: c_int,
        n: usize,
        size: usize,
    ) -> *mut c_void {
        if n > size {
            crate::chk_fail("memset");
        }
        // SAFETY: checked above, then forwarded.
        unsafe { memset(dst, byte, n) }
    }

    /// # Safety
    /// C ABI: as `strcpy`, plus `size` describing `dst`.
    #[no_mangle]
    pub unsafe extern "C" fn __strcpy_chk(
        dst: *mut c_char,
        src: *const c_char,
        size: usize,
    ) -> *mut c_char {
        // SAFETY: forwarded from the caller.
        if unsafe { super::strlen(src) } + 1 > size {
            crate::chk_fail("strcpy");
        }
        // SAFETY: checked above, then forwarded.
        unsafe { strcpy(dst, src) }
    }

    /// # Safety
    /// C ABI: as `strcat`, plus `size` describing `dst`.
    #[no_mangle]
    pub unsafe extern "C" fn __strcat_chk(
        dst: *mut c_char,
        src: *const c_char,
        size: usize,
    ) -> *mut c_char {
        // SAFETY: forwarded from the caller.
        if unsafe { super::strlen(dst) + super::strlen(src) } + 1 > size {
            crate::chk_fail("strcat");
        }
        // SAFETY: checked above, then forwarded.
        unsafe { strcat(dst, src) }
    }

    /// # Safety
    /// C ABI: as `strncat`, plus `size` describing `dst`.
    #[no_mangle]
    pub unsafe extern "C" fn __strncat_chk(
        dst: *mut c_char,
        src: *const c_char,
        n: usize,
        size: usize,
    ) -> *mut c_char {
        // SAFETY: forwarded from the caller.
        let need = unsafe { super::strlen(dst) + super::strlen(src).min(n) } + 1;
        if need > size {
            crate::chk_fail("strncat");
        }
        // SAFETY: checked above, then forwarded.
        unsafe { strncat(dst, src, n) }
    }

    /// # Safety
    /// C ABI: as `strncpy`, plus `size` describing `dst`.
    #[no_mangle]
    pub unsafe extern "C" fn __strncpy_chk(
        dst: *mut c_char,
        src: *const c_char,
        n: usize,
        size: usize,
    ) -> *mut c_char {
        if n > size {
            crate::chk_fail("strncpy");
        }
        // SAFETY: checked above, then forwarded.
        unsafe { strncpy(dst, src, n) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> &[u8] {
        text.as_bytes()
    }

    #[test]
    fn strtoul_keeps_the_range_strtol_saturates() {
        // The whole reason `strtoul` is not a wrapper. Every one of these is
        // representable as `u64` and larger than `i64::MAX`, so `strtol` answers
        // `i64::MAX` for all four and cannot tell them apart.
        for (text, want) in [
            ("9223372036854775808", 9_223_372_036_854_775_808u64),
            ("18446744073709551615", u64::MAX),
            ("0xffffffffffffffff", u64::MAX),
            ("0xdeadbeefcafebabe", 0xdead_beef_cafe_babe),
        ] {
            let (value, used) = strtoul(text.as_bytes(), 0);
            assert_eq!(value, want, "strtoul({text})");
            assert_eq!(used, text.len(), "and consumed all of it");
            assert_eq!(strtol(text.as_bytes(), 0).0, i64::MAX, "strtol saturates {text}");
        }
    }

    #[test]
    fn strtoul_negates_because_c_says_so() {
        // `strtoul("-1")` is `ULONG_MAX`, not an error. Genuinely strange, and the
        // standard's — a program relying on it exists somewhere, and refusing would
        // be this library inventing a rule.
        assert_eq!(strtoul(b"-1", 10).0, u64::MAX);
        assert_eq!(strtoul(b"-2", 10).0, u64::MAX - 1);
    }

    #[test]
    fn strtoul_agrees_with_strtol_where_both_fit() {
        for text in ["0", "1", "42", "  -17", "0x1f", "017", "9223372036854775807"] {
            let signed = strtol(text.as_bytes(), 0);
            let unsigned = strtoul(text.as_bytes(), 0);
            assert_eq!(unsigned.1, signed.1, "same bytes consumed for {text}");
            assert_eq!(unsigned.0, signed.0 as u64, "same value for {text}");
        }
        // No digits: zero consumed, which is how a caller tells "0" from "not a
        // number" — the same contract `strtol` has.
        assert_eq!(strtoul(b"zzz", 10), (0, 0));
    }

    #[test]
    fn compare_is_unsigned() {
        // The bug this test exists for: `c_char` is signed on aarch64, so a naive
        // implementation makes 0xC3 (the first byte of "Ä" in UTF-8) compare as
        // negative and sort *below* every ASCII byte. Then "Ä" < "A", and a sorted
        // container of file names silently misorders every non-ASCII entry.
        assert!(compare(s("Ä"), s("A")) > 0);
        assert!(compare(&[0xff], &[0x01]) > 0);
        assert_eq!(compare(s("abc"), s("abc")), 0);
    }

    #[test]
    fn strcmp_handles_prefixes() {
        assert!(strcmp(s("abc"), s("abcd")) < 0);
        assert!(strcmp(s("abcd"), s("abc")) > 0);
        assert_eq!(strcmp(s(""), s("")), 0);
        assert!(strcmp(s(""), s("a")) < 0);
    }

    #[test]
    fn strncmp_stops_at_n() {
        assert_eq!(strncmp(s("abcX"), s("abcY"), 3), 0);
        assert!(strncmp(s("abcX"), s("abcY"), 4) < 0);
        // A bound longer than either string must still compare the strings, not
        // whatever follows them.
        assert!(strncmp(s("ab"), s("abc"), 99) < 0);
    }

    #[test]
    fn find_substring() {
        assert_eq!(find(s("hello from the initramfs"), s("from")), Some(6));
        assert_eq!(find(s("abc"), s("")), Some(0));
        assert_eq!(find(s("abc"), s("abcd")), None);
        assert_eq!(find(s("aaab"), s("ab")), Some(2));
    }

    #[test]
    fn spans() {
        assert_eq!(complement_span(s("path/to/file"), s("/")), 4);
        assert_eq!(complement_span(s("nothing"), s("/")), 7);
        assert_eq!(span(s("   x"), s(" ")), 3);
    }

    #[test]
    fn strtol_bases_and_signs() {
        assert_eq!(strtol(s("42"), 10), (42, 2));
        assert_eq!(strtol(s("  -17rest"), 10), (-17, 5));
        assert_eq!(strtol(s("0x1f"), 16), (31, 4));
        assert_eq!(strtol(s("0x1f"), 0), (31, 4));
        assert_eq!(strtol(s("017"), 0), (15, 3));
        assert_eq!(strtol(s("17"), 0), (17, 2));
        // "0x" with no digit after it is the number 0 followed by 'x'.
        assert_eq!(strtol(s("0xz"), 0), (0, 1));
        // Nothing numeric: zero consumed, so a caller can tell this from "0".
        assert_eq!(strtol(s("hello"), 10), (0, 0));
    }

    #[test]
    fn strtol_saturates_instead_of_wrapping() {
        // Wrapping here turns a too-large timeout into a small one — a bug that
        // survives every test that only checks small numbers.
        let (v, used) = strtol(s("99999999999999999999999"), 10);
        assert_eq!(v, i64::MAX);
        assert_eq!(used, 23);
        assert_eq!(strtol(s("-99999999999999999999999"), 10).0, i64::MIN);
    }
}
