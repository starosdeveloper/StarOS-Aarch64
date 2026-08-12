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
    use super::{as_bytes, c_char, c_int, c_void, digit};

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
    pub extern "C" fn isspace(c: c_int) -> c_int {
        c_int::from(c == 0x20 || (0x09..=0x0d).contains(&c))
    }

    #[no_mangle]
    pub extern "C" fn isdigit(c: c_int) -> c_int {
        c_int::from(digit(c as u8, 10).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> &[u8] {
        text.as_bytes()
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
