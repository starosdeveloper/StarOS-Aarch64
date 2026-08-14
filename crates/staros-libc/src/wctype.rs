//! `<wctype.h>` — classifying wide characters, in the "C" locale and no other.
//!
//! Found by QML: `<cwctype>` is what libstdc++ pulls in for QV4's lexer, so the
//! JavaScript engine does not compile without these nineteen names. That makes them
//! different from the `wcs*` string family, which is declared and left undefined on
//! purpose — those are required to *exist* by libstdc++ and called by nothing here,
//! while these are called.
//!
//! ## What "the C locale" means for a character above 127
//!
//! It classifies as nothing. Not "unknown", not "probably a letter" — false to every
//! question, and unchanged by `towlower`. That is what the C locale *is*, and it is
//! the honest answer rather than a small one: deciding whether U+00E9 is a letter
//! needs the Unicode character database, which is a real subsystem with a real table
//! and not something to approximate. An approximation would be worse than the
//! refusal, because a lexer that believed a wrong answer accepts identifiers it
//! should reject and rejects ones it should accept, and both look like bugs in the
//! program being run.
//!
//! [`crate::locale`] accepts only `"C"`, so there is no second answer to switch to
//! and nothing here has to ask which locale is current.
//!
//! ## `wctype` and `wctrans`, the indirect forms
//!
//! `iswctype(c, wctype("alpha"))` is `iswalpha(c)` with the class chosen at run
//! time. The handle is a small integer here rather than a pointer into a table,
//! which is what makes the whole thing `const`-evaluable and the failure — an
//! unknown class name — a zero that classifies nothing.

use core::ffi::c_int;

/// The wide character type, matching this target's ABI: 32 bits, unsigned.
pub type WInt = u32;

/// The classes `wctype` can name, as the handles it returns. Zero is "no such
/// class", which is what C specifies for an unknown name and what makes
/// `iswctype(c, wctype("nonsense"))` answer false rather than something arbitrary.
const CLASS_NONE: c_int = 0;
const CLASS_ALNUM: c_int = 1;
const CLASS_ALPHA: c_int = 2;
const CLASS_BLANK: c_int = 3;
const CLASS_CNTRL: c_int = 4;
const CLASS_DIGIT: c_int = 5;
const CLASS_GRAPH: c_int = 6;
const CLASS_LOWER: c_int = 7;
const CLASS_PRINT: c_int = 8;
const CLASS_PUNCT: c_int = 9;
const CLASS_SPACE: c_int = 10;
const CLASS_UPPER: c_int = 11;
const CLASS_XDIGIT: c_int = 12;

/// The two mappings `wctrans` can name.
const TRANS_NONE: c_int = 0;
const TRANS_LOWER: c_int = 1;
const TRANS_UPPER: c_int = 2;

/// A wide character as a byte, when it is one.
///
/// `WEOF` is `0xffff_ffff` and every value above 127 answers `None`, which is what
/// makes both classify as nothing without a special case for either.
///
/// The bound is **not falsifiable from outside**, and saying so is better than
/// implying it is load-bearing: [`crate::ctype`]'s ranges are ASCII already, so
/// widening this to 256 changes no answer. Moving it was tried and nothing failed.
/// It stays because it states the intent at this layer — a wide character is not a
/// byte, and the day a Unicode table exists this is the line that has to change.
fn narrow(c: WInt) -> Option<c_int> {
    (c < 128).then_some(c as c_int)
}

pub(crate) fn is_class(c: WInt, class: c_int) -> bool {
    let Some(b) = narrow(c) else { return false };
    match class {
        CLASS_ALNUM => crate::ctype::is_alnum(b),
        CLASS_ALPHA => crate::ctype::is_alpha(b),
        CLASS_BLANK => crate::ctype::is_blank(b),
        CLASS_CNTRL => crate::ctype::is_cntrl(b),
        CLASS_DIGIT => crate::ctype::is_digit(b),
        CLASS_GRAPH => crate::ctype::is_graph(b),
        CLASS_LOWER => crate::ctype::is_lower(b),
        CLASS_PRINT => crate::ctype::is_print(b),
        CLASS_PUNCT => crate::ctype::is_punct(b),
        CLASS_SPACE => crate::ctype::is_space(b),
        CLASS_UPPER => crate::ctype::is_upper(b),
        CLASS_XDIGIT => crate::ctype::is_xdigit(b),
        _ => false,
    }
}

pub(crate) fn to_lower(c: WInt) -> WInt {
    match narrow(c) {
        Some(b) => crate::ctype::to_lower(b) as WInt,
        None => c,
    }
}

pub(crate) fn to_upper(c: WInt) -> WInt {
    match narrow(c) {
        Some(b) => crate::ctype::to_upper(b) as WInt,
        None => c,
    }
}

/// Look a class name up. The names are POSIX's twelve and nothing else.
pub(crate) fn class_of(name: &[u8]) -> c_int {
    match name {
        b"alnum" => CLASS_ALNUM,
        b"alpha" => CLASS_ALPHA,
        b"blank" => CLASS_BLANK,
        b"cntrl" => CLASS_CNTRL,
        b"digit" => CLASS_DIGIT,
        b"graph" => CLASS_GRAPH,
        b"lower" => CLASS_LOWER,
        b"print" => CLASS_PRINT,
        b"punct" => CLASS_PUNCT,
        b"space" => CLASS_SPACE,
        b"upper" => CLASS_UPPER,
        b"xdigit" => CLASS_XDIGIT,
        _ => CLASS_NONE,
    }
}

pub(crate) fn trans_of(name: &[u8]) -> c_int {
    match name {
        b"tolower" => TRANS_LOWER,
        b"toupper" => TRANS_UPPER,
        _ => TRANS_NONE,
    }
}

pub(crate) fn transform(c: WInt, trans: c_int) -> WInt {
    match trans {
        TRANS_LOWER => to_lower(c),
        TRANS_UPPER => to_upper(c),
        _ => c,
    }
}

/// The C entry points.
#[cfg(not(test))]
pub mod exports {
    use super::{c_int, WInt};

    macro_rules! classify {
        ($($name:ident => $class:expr),* $(,)?) => {$(
            #[no_mangle]
            pub extern "C" fn $name(c: WInt) -> c_int {
                c_int::from(super::is_class(c, $class))
            }
        )*};
    }

    classify! {
        iswalnum => super::CLASS_ALNUM,
        iswalpha => super::CLASS_ALPHA,
        iswblank => super::CLASS_BLANK,
        iswcntrl => super::CLASS_CNTRL,
        iswdigit => super::CLASS_DIGIT,
        iswgraph => super::CLASS_GRAPH,
        iswlower => super::CLASS_LOWER,
        iswprint => super::CLASS_PRINT,
        iswpunct => super::CLASS_PUNCT,
        iswspace => super::CLASS_SPACE,
        iswupper => super::CLASS_UPPER,
        iswxdigit => super::CLASS_XDIGIT,
    }

    #[no_mangle]
    pub extern "C" fn towlower(c: WInt) -> WInt {
        super::to_lower(c)
    }

    #[no_mangle]
    pub extern "C" fn towupper(c: WInt) -> WInt {
        super::to_upper(c)
    }

    #[no_mangle]
    pub extern "C" fn iswctype(c: WInt, class: c_int) -> c_int {
        c_int::from(super::is_class(c, class))
    }

    #[no_mangle]
    pub extern "C" fn towctrans(c: WInt, trans: c_int) -> WInt {
        super::transform(c, trans)
    }

    /// # Safety
    /// C ABI: `name` is a NUL-terminated string.
    #[no_mangle]
    pub unsafe extern "C" fn wctype(name: *const core::ffi::c_char) -> c_int {
        if name.is_null() {
            return 0;
        }
        // SAFETY: forwarded from the caller.
        super::class_of(unsafe { crate::string::as_bytes(name) })
    }

    /// # Safety
    /// As [`wctype`].
    #[no_mangle]
    pub unsafe extern "C" fn wctrans(name: *const core::ffi::c_char) -> c_int {
        if name.is_null() {
            return 0;
        }
        // SAFETY: forwarded from the caller.
        super::trans_of(unsafe { crate::string::as_bytes(name) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_classifies_as_it_does_narrow() {
        // The wide answers must be the narrow ones for every ASCII value, or a
        // program that switches between `isalpha` and `iswalpha` — which a lexer
        // does at the boundary between its fast path and its slow one — sees the
        // same character two different ways.
        for c in 0..128u32 {
            let b = c as c_int;
            assert_eq!(is_class(c, CLASS_ALPHA), crate::ctype::is_alpha(b), "iswalpha({c})");
            assert_eq!(is_class(c, CLASS_DIGIT), crate::ctype::is_digit(b), "iswdigit({c})");
            assert_eq!(is_class(c, CLASS_SPACE), crate::ctype::is_space(b), "iswspace({c})");
            assert_eq!(to_lower(c), crate::ctype::to_lower(b) as u32, "towlower({c})");
            assert_eq!(to_upper(c), crate::ctype::to_upper(b) as u32, "towupper({c})");
        }
    }

    #[test]
    fn everything_above_ascii_classifies_as_nothing_and_is_left_alone() {
        // The C locale's answer, and the one this system can give honestly. A
        // guess here would have a lexer accept identifiers it should reject.
        for c in [0xa9u32, 0xe9, 0x0416, 0x4e2d, 0x1f600, u32::MAX] {
            for class in CLASS_ALNUM..=CLASS_XDIGIT {
                assert!(!is_class(c, class), "class {class} of U+{c:04X}");
            }
            assert_eq!(to_lower(c), c, "towlower leaves U+{c:04X} alone");
            assert_eq!(to_upper(c), c, "towupper leaves U+{c:04X} alone");
        }
    }

    #[test]
    fn the_named_classes_answer_as_their_functions_do() {
        assert_eq!(class_of(b"alpha"), CLASS_ALPHA);
        assert_eq!(class_of(b"xdigit"), CLASS_XDIGIT);
        assert!(is_class(u32::from(b'f'), class_of(b"xdigit")));
        assert!(!is_class(u32::from(b'g'), class_of(b"xdigit")));
        // An unknown name is zero, and zero classifies nothing — so a caller that
        // mistyped gets a consistent false rather than whichever class happened to
        // be first in a table.
        assert_eq!(class_of(b"nonsense"), CLASS_NONE);
        assert!(!is_class(u32::from(b'a'), class_of(b"nonsense")));
    }

    #[test]
    fn the_named_transforms_do_what_their_functions_do() {
        assert_eq!(transform(u32::from(b'Q'), trans_of(b"tolower")), u32::from(b'q'));
        assert_eq!(transform(u32::from(b'q'), trans_of(b"toupper")), u32::from(b'Q'));
        assert_eq!(transform(u32::from(b'q'), trans_of(b"nonsense")), u32::from(b'q'));
    }
}
