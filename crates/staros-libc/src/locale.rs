//! One locale, named "C".
//!
//! Three of the contract's symbols are here — `setlocale`, `nl_langinfo` and
//! `localeconv` — and all three are answers to the same question: what does this
//! system think a number, a date and a byte look like? The answer is the C locale,
//! because there is no locale database on this machine and no environment to read a
//! preference from.
//!
//! What matters is that the refusal is legible. `setlocale(LC_ALL, "de_DE.UTF-8")`
//! returns null rather than "C": a caller that gets a non-null answer is entitled to
//! believe the locale it asked for is in effect, and a program formatting German
//! prices with an English decimal separator is a bug that surfaces in a screenshot
//! rather than at the call.

use core::ffi::{c_char, c_int};

/// The locale's name, and the only string `setlocale` ever returns.
const C_LOCALE: &[u8] = b"C\0";

/// Is this the locale we have?
///
/// `""` means "whatever the environment says", and the environment here says
/// nothing, so it resolves to C — which is what an empty environment means on any
/// system. `"C"` and `"POSIX"` are the same locale under two names.
pub(crate) fn is_c_locale(name: &[u8]) -> bool {
    name.is_empty() || name == b"C" || name == b"POSIX" || name == b"C.UTF-8"
}

/// The C entry points.
#[cfg(not(test))]
pub mod exports {
    use super::{c_char, c_int, is_c_locale, C_LOCALE};

    /// The numeric and monetary conventions of the C locale, in the layout
    /// `<locale.h>` declares. `CHAR_MAX` in a `char` field means "not available",
    /// which is how C says a value is unspecified rather than zero.
    #[repr(C)]
    pub struct Lconv {
        decimal_point: *const c_char,
        thousands_sep: *const c_char,
        grouping: *const c_char,
        int_curr_symbol: *const c_char,
        currency_symbol: *const c_char,
        mon_decimal_point: *const c_char,
        mon_thousands_sep: *const c_char,
        mon_grouping: *const c_char,
        positive_sign: *const c_char,
        negative_sign: *const c_char,
        int_frac_digits: c_char,
        frac_digits: c_char,
        p_cs_precedes: c_char,
        p_sep_by_space: c_char,
        n_cs_precedes: c_char,
        n_sep_by_space: c_char,
        p_sign_posn: c_char,
        n_sign_posn: c_char,
    }

    // SAFETY: every pointer in it is to a `'static` byte string in `.rodata`, and
    // nothing ever writes to the structure.
    unsafe impl Sync for Lconv {}

    const POINT: &[u8] = b".\0";
    const EMPTY: &[u8] = b"\0";
    const CHAR_MAX: c_char = 127;

    static C_LCONV: Lconv = Lconv {
        decimal_point: POINT.as_ptr().cast::<c_char>(),
        thousands_sep: EMPTY.as_ptr().cast::<c_char>(),
        grouping: EMPTY.as_ptr().cast::<c_char>(),
        int_curr_symbol: EMPTY.as_ptr().cast::<c_char>(),
        currency_symbol: EMPTY.as_ptr().cast::<c_char>(),
        mon_decimal_point: EMPTY.as_ptr().cast::<c_char>(),
        mon_thousands_sep: EMPTY.as_ptr().cast::<c_char>(),
        mon_grouping: EMPTY.as_ptr().cast::<c_char>(),
        positive_sign: EMPTY.as_ptr().cast::<c_char>(),
        negative_sign: EMPTY.as_ptr().cast::<c_char>(),
        int_frac_digits: CHAR_MAX,
        frac_digits: CHAR_MAX,
        p_cs_precedes: CHAR_MAX,
        p_sep_by_space: CHAR_MAX,
        n_cs_precedes: CHAR_MAX,
        n_sep_by_space: CHAR_MAX,
        p_sign_posn: CHAR_MAX,
        n_sign_posn: CHAR_MAX,
    };

    /// # Safety
    /// C ABI: `locale` is null (a query) or a NUL-terminated name.
    #[no_mangle]
    pub unsafe extern "C" fn setlocale(_category: c_int, locale: *const c_char) -> *mut c_char {
        if locale.is_null() {
            // A query: what is in effect. Always C.
            return C_LOCALE.as_ptr().cast::<c_char>().cast_mut();
        }
        // SAFETY: forwarded from the caller.
        let name = unsafe { crate::string::as_bytes(locale) };
        if is_c_locale(name) {
            C_LOCALE.as_ptr().cast::<c_char>().cast_mut()
        } else {
            core::ptr::null_mut()
        }
    }

    #[no_mangle]
    pub extern "C" fn localeconv() -> *const Lconv {
        core::ptr::addr_of!(C_LCONV)
    }

    /// `nl_langinfo`, for the items a program actually asks about.
    ///
    /// `CODESET` is the interesting one and the answer is "UTF-8" rather than the C
    /// locale's nominal "ANSI_X3.4-1968". That is a deliberate choice about this
    /// system rather than a copy of glibc's table: the console, the file server and
    /// the initramfs all carry bytes through unchanged, so a program that encodes
    /// its text as UTF-8 gets exactly what it wrote back out. Saying "ASCII" here
    /// would make Qt transcode every non-ASCII string into question marks.
    #[no_mangle]
    pub extern "C" fn nl_langinfo(item: c_int) -> *mut c_char {
        const CODESET: c_int = 14;
        const D_T_FMT: c_int = 131_112;
        const D_FMT: c_int = 131_113;
        const T_FMT: c_int = 131_114;
        const AM_STR: c_int = 131_110;
        const PM_STR: c_int = 131_111;
        const RADIXCHAR: c_int = 65_536;
        const THOUSEP: c_int = 65_537;
        const YESEXPR: c_int = 327_680;
        const NOEXPR: c_int = 327_681;

        let text: &[u8] = match item {
            CODESET => b"UTF-8\0",
            D_T_FMT => b"%a %b %e %H:%M:%S %Y\0",
            D_FMT => b"%m/%d/%y\0",
            T_FMT => b"%H:%M:%S\0",
            AM_STR => b"AM\0",
            PM_STR => b"PM\0",
            RADIXCHAR => b".\0",
            THOUSEP => EMPTY,
            YESEXPR => b"^[yY]\0",
            NOEXPR => b"^[nN]\0",
            // C says an unknown item yields an empty string, not null. A caller
            // that dereferences the result — and they all do — must not fault.
            _ => EMPTY,
        };
        text.as_ptr().cast::<c_char>().cast_mut()
    }

    /// The `_l` locale-object family, which libstdc++ calls into. There is one
    /// locale and no object behind the handle, so creating one yields a non-null
    /// token that means "C" and using it changes nothing.
    ///
    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn newlocale(
        _mask: c_int,
        _name: *const c_char,
        _base: *mut core::ffi::c_void,
    ) -> *mut core::ffi::c_void {
        // A distinctive non-null token: if one of these ever reaches something that
        // dereferences it, the fault address says where it came from.
        1 as *mut core::ffi::c_void
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn uselocale(_loc: *mut core::ffi::c_void) -> *mut core::ffi::c_void {
        1 as *mut core::ffi::c_void
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn freelocale(_loc: *mut core::ffi::c_void) {}

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn duplocale(_loc: *mut core::ffi::c_void) -> *mut core::ffi::c_void {
        1 as *mut core::ffi::c_void
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_c_locale_is_accepted() {
        for name in [&b""[..], b"C", b"POSIX", b"C.UTF-8"] {
            assert!(is_c_locale(name), "{name:?} should be the C locale");
        }
        // The refusals. Each of these is a locale a real program asks for, and
        // answering "C" to any of them is a silently wrong decimal separator.
        for name in [&b"de_DE.UTF-8"[..], b"en_US", b"ru_RU.UTF-8", b"c"] {
            assert!(!is_c_locale(name), "{name:?} should be refused");
        }
    }
}
