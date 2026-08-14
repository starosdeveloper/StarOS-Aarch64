//! `<ctype.h>` — character classification, in the "C" locale and no other.
//!
//! Fourteen functions this sysroot's header had declared since it was written and
//! nobody had implemented. Nothing noticed, because the symbol contract in
//! `docs/LIBC-CONTRACT.md` is measured from what a Qt *link* leaves undefined, and
//! Qt resolves these against the libm it was built with. A C++ program compiling
//! against these headers is a different demand: libstdc++'s `<cctype>` says
//! `using ::isalpha;`, and that fails at compile time in a file that has nothing to
//! do with the mistake.
//!
//! ## Why the argument is an `int` and why that matters
//!
//! C says the argument must be representable as `unsigned char`, or be `EOF`.
//! Anything else is undefined, and glibc's implementation — a table indexed from
//! -128 — really does read out of bounds for it. There is no table here, so the
//! range check is a comparison and costs nothing; a value outside it answers
//! "false" rather than reading memory it was not given.
//!
//! ## One locale
//!
//! `setlocale` accepts only `"C"` (see [`crate::locale`]), so these are the ASCII
//! answers and there is no table to switch. A locale-aware `isalpha` would have to
//! decide what to do with byte 0xE9 in a UTF-8 world, and the answer for a single
//! byte is "it is not a character", which is what returning false already says.

use core::ffi::c_int;

/// Is `c` a byte value this can classify at all?
///
/// `EOF` is -1 and answers false to every classification, which is what a loop
/// like `while (isspace(c = getchar()))` depends on.
fn byte(c: c_int) -> Option<u8> {
    u8::try_from(c).ok()
}

pub(crate) fn is_digit(c: c_int) -> bool {
    matches!(byte(c), Some(b'0'..=b'9'))
}

pub(crate) fn is_lower(c: c_int) -> bool {
    matches!(byte(c), Some(b'a'..=b'z'))
}

pub(crate) fn is_upper(c: c_int) -> bool {
    matches!(byte(c), Some(b'A'..=b'Z'))
}

pub(crate) fn is_alpha(c: c_int) -> bool {
    is_lower(c) || is_upper(c)
}

pub(crate) fn is_alnum(c: c_int) -> bool {
    is_alpha(c) || is_digit(c)
}

pub(crate) fn is_xdigit(c: c_int) -> bool {
    matches!(byte(c), Some(b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F'))
}

/// Space, in the sense C means: the six characters that separate tokens.
pub(crate) fn is_space(c: c_int) -> bool {
    matches!(byte(c), Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
}

/// Blank, which is *not* space: only the two that can occur inside a line. The
/// difference is why `isblank` exists at all, and a version that forwarded to
/// `isspace` would make `\n` blank and every line-oriented parser wrong.
pub(crate) fn is_blank(c: c_int) -> bool {
    matches!(byte(c), Some(b' ' | b'\t'))
}

pub(crate) fn is_cntrl(c: c_int) -> bool {
    matches!(byte(c), Some(0x00..=0x1f | 0x7f))
}

/// Printing characters other than space — every graphic mark.
pub(crate) fn is_graph(c: c_int) -> bool {
    matches!(byte(c), Some(0x21..=0x7e))
}

/// Printing characters *including* space, which is the whole difference from
/// `isgraph` and the reason both exist.
pub(crate) fn is_print(c: c_int) -> bool {
    matches!(byte(c), Some(0x20..=0x7e))
}

/// Printing, not alphanumeric, not space: punctuation by elimination, which is how
/// C defines it rather than by listing the marks.
pub(crate) fn is_punct(c: c_int) -> bool {
    is_graph(c) && !is_alnum(c)
}

pub(crate) fn to_lower(c: c_int) -> c_int {
    if is_upper(c) {
        c + 32
    } else {
        c
    }
}

pub(crate) fn to_upper(c: c_int) -> c_int {
    if is_lower(c) {
        c - 32
    } else {
        c
    }
}

/// glibc's classification bits, in the order its header assigns them.
///
/// `_ISbit(n)` is `(1 << n) << 8` for the first eight and `(1 << n) >> 8` after —
/// a byte-swapped layout that exists because glibc stores the table in network
/// order on some targets. The numbers are copied rather than derived: they are an
/// ABI, and a table whose bits were assigned differently would classify every
/// character wrongly through `std::ctype` while `isalpha` kept working.
const fn bit(n: u32) -> u16 {
    if n < 8 {
        ((1u32 << n) << 8) as u16
    } else {
        ((1u32 << n) >> 8) as u16
    }
}

const IS_UPPER: u16 = bit(0);
const IS_LOWER: u16 = bit(1);
const IS_ALPHA: u16 = bit(2);
const IS_DIGIT: u16 = bit(3);
const IS_XDIGIT: u16 = bit(4);
const IS_SPACE: u16 = bit(5);
const IS_PRINT: u16 = bit(6);
const IS_GRAPH: u16 = bit(7);
const IS_BLANK: u16 = bit(8);
const IS_CNTRL: u16 = bit(9);
const IS_PUNCT: u16 = bit(10);
const IS_ALNUM: u16 = bit(11);

/// The mask for one byte value.
const fn mask_of(c: u8) -> u16 {
    let mut m = 0u16;
    if c.is_ascii_uppercase() {
        m |= IS_UPPER | IS_ALPHA | IS_ALNUM;
    }
    if c.is_ascii_lowercase() {
        m |= IS_LOWER | IS_ALPHA | IS_ALNUM;
    }
    if c.is_ascii_digit() {
        m |= IS_DIGIT | IS_ALNUM | IS_XDIGIT;
    }
    if matches!(c, b'a'..=b'f' | b'A'..=b'F') {
        m |= IS_XDIGIT;
    }
    if matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        m |= IS_SPACE;
    }
    if matches!(c, b' ' | b'\t') {
        m |= IS_BLANK;
    }
    if c.is_ascii_control() {
        m |= IS_CNTRL;
    }
    if c.is_ascii_graphic() {
        m |= IS_GRAPH;
    }
    if c >= 0x20 && c < 0x7f {
        m |= IS_PRINT;
    }
    if c.is_ascii_punctuation() {
        m |= IS_PUNCT;
    }
    m
}

/// The table libstdc++ indexes, 384 entries wide.
///
/// It runs from -128 to 255 because a signed `char` is used as the index directly.
/// The first 128 entries — the negative half — are zero: this is the C locale, and
/// a byte above 127 classifies as nothing. Getting the offset wrong would make
/// every high byte a letter, which surfaces months later as a parser accepting
/// rubbish.
const fn build_masks() -> [u16; 384] {
    let mut table = [0u16; 384];
    let mut i = 0;
    while i < 256 {
        table[128 + i] = mask_of(i as u8);
        i += 1;
    }
    table
}

static CTYPE_MASKS: [u16; 384] = build_masks();

const fn build_case(upper: bool) -> [i32; 384] {
    let mut table = [0i32; 384];
    let mut i = 0;
    while i < 256 {
        let c = i as u8;
        let mapped = if upper {
            if c.is_ascii_lowercase() {
                c - 32
            } else {
                c
            }
        } else if c.is_ascii_uppercase() {
            c + 32
        } else {
            c
        };
        table[128 + i] = mapped as i32;
        // The negative half maps to itself: those indices are bytes above 127 seen
        // as signed, and the C locale changes the case of none of them.
        table[i] = (i as i32) - 128;
        i += 1;
    }
    table
}

static CTYPE_TOLOWER: [i32; 384] = build_case(false);
static CTYPE_TOUPPER: [i32; 384] = build_case(true);

/// The C entry points.
#[cfg(not(test))]
pub mod exports {
    use super::c_int;

    macro_rules! classify {
        ($($name:ident => $imp:path),* $(,)?) => {$(
            #[no_mangle]
            pub extern "C" fn $name(c: c_int) -> c_int { c_int::from($imp(c)) }
        )*};
    }

    classify! {
        isalnum => super::is_alnum,
        isalpha => super::is_alpha,
        isblank => super::is_blank,
        iscntrl => super::is_cntrl,
        isdigit => super::is_digit,
        isgraph => super::is_graph,
        islower => super::is_lower,
        isprint => super::is_print,
        ispunct => super::is_punct,
        isspace => super::is_space,
        isupper => super::is_upper,
        isxdigit => super::is_xdigit,
    }

    #[no_mangle]
    pub extern "C" fn tolower(c: c_int) -> c_int {
        super::to_lower(c)
    }

    #[no_mangle]
    pub extern "C" fn toupper(c: c_int) -> c_int {
        super::to_upper(c)
    }

    // The three `*_loc` functions return a pointer *to a pointer* to the table.
    //
    // The extra indirection is glibc's, and it is there so a thread can have its
    // own locale: the inner pointer is thread-local there and changes when
    // `uselocale` is called. There is one locale here and no `uselocale`, so the
    // cell is a static and every thread sees the same table — which is correct
    // rather than a simplification, because with one locale there is nothing for
    // two threads to disagree about.

    static MASKS: &[u16; 384] = &super::CTYPE_MASKS;
    static TOLOWER: &[i32; 384] = &super::CTYPE_TOLOWER;
    static TOUPPER: &[i32; 384] = &super::CTYPE_TOUPPER;

    /// The cells the `*_loc` functions hand out. Each holds the address of the
    /// table's *zero* entry, so `table[-1]` is the byte 0xff seen as signed — which
    /// is the whole reason the tables are 384 wide and start at -128.
    static MASK_CELL: MaskCell = MaskCell(core::sync::atomic::AtomicPtr::new(
        core::ptr::null_mut(),
    ));
    static LOWER_CELL: CaseCell = CaseCell(core::sync::atomic::AtomicPtr::new(
        core::ptr::null_mut(),
    ));
    static UPPER_CELL: CaseCell = CaseCell(core::sync::atomic::AtomicPtr::new(
        core::ptr::null_mut(),
    ));

    struct MaskCell(core::sync::atomic::AtomicPtr<u16>);
    struct CaseCell(core::sync::atomic::AtomicPtr<i32>);
    // SAFETY: the pointer only ever holds the address of a `'static` table, and
    // every write stores the same value.
    unsafe impl Sync for MaskCell {}
    unsafe impl Sync for CaseCell {}

    #[no_mangle]
    pub extern "C" fn __ctype_b_loc() -> *mut *const u16 {
        // SAFETY: `MASKS` is `'static`; offsetting to entry 128 is inside it, and
        // the pointer handed out is only ever indexed from -128 to 255.
        let base = unsafe { MASKS.as_ptr().add(128) };
        MASK_CELL.0.store(base.cast_mut(), core::sync::atomic::Ordering::Relaxed);
        MASK_CELL.0.as_ptr().cast::<*const u16>()
    }

    #[no_mangle]
    pub extern "C" fn __ctype_tolower_loc() -> *mut *const c_int {
        // SAFETY: as above.
        let base = unsafe { TOLOWER.as_ptr().add(128) };
        LOWER_CELL.0.store(base.cast_mut(), core::sync::atomic::Ordering::Relaxed);
        LOWER_CELL.0.as_ptr().cast::<*const c_int>()
    }

    #[no_mangle]
    pub extern "C" fn __ctype_toupper_loc() -> *mut *const c_int {
        // SAFETY: as above.
        let base = unsafe { TOUPPER.as_ptr().add(128) };
        UPPER_CELL.0.store(base.cast_mut(), core::sync::atomic::Ordering::Relaxed);
        UPPER_CELL.0.as_ptr().cast::<*const c_int>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_classes_agree_with_ascii() {
        for c in 0..128i32 {
            let ch = c as u8 as char;
            assert_eq!(is_alpha(c), ch.is_ascii_alphabetic(), "isalpha({c})");
            assert_eq!(is_digit(c), ch.is_ascii_digit(), "isdigit({c})");
            assert_eq!(is_alnum(c), ch.is_ascii_alphanumeric(), "isalnum({c})");
            assert_eq!(is_xdigit(c), ch.is_ascii_hexdigit(), "isxdigit({c})");
            assert_eq!(is_lower(c), ch.is_ascii_lowercase(), "islower({c})");
            assert_eq!(is_upper(c), ch.is_ascii_uppercase(), "isupper({c})");
            assert_eq!(is_punct(c), ch.is_ascii_punctuation(), "ispunct({c})");
            assert_eq!(is_graph(c), ch.is_ascii_graphic(), "isgraph({c})");
            assert_eq!(is_cntrl(c), ch.is_ascii_control(), "iscntrl({c})");
            assert_eq!(is_space(c), ch.is_ascii_whitespace() || c == 0x0b, "isspace({c})");
        }
    }

    #[test]
    fn blank_is_not_space() {
        // The distinction the two names exist for. A newline is space and is not
        // blank, and a version that forwarded one to the other would make every
        // line-oriented parser read past the end of its line.
        assert!(is_space(i32::from(b'\n')));
        assert!(!is_blank(i32::from(b'\n')));
        assert!(is_blank(i32::from(b' ')));
        assert!(is_blank(i32::from(b'\t')));
    }

    #[test]
    fn eof_and_out_of_range_classify_as_nothing() {
        // `while (isspace(c = getchar()))` depends on EOF answering false, and a
        // table-driven implementation indexed from -128 would read out of bounds
        // for the values below instead.
        for c in [-1, -129, 256, 1000, i32::MIN, i32::MAX] {
            assert!(!is_alpha(c) && !is_digit(c) && !is_space(c), "classify({c})");
            assert_eq!(to_lower(c), c, "tolower({c}) leaves it alone");
            assert_eq!(to_upper(c), c, "toupper({c}) leaves it alone");
        }
    }

    #[test]
    fn case_conversion_is_a_round_trip_for_letters_and_identity_for_the_rest() {
        for c in 0..128i32 {
            let ch = c as u8 as char;
            assert_eq!(to_lower(c), i32::from(ch.to_ascii_lowercase() as u8));
            assert_eq!(to_upper(c), i32::from(ch.to_ascii_uppercase() as u8));
            if ch.is_ascii_alphabetic() {
                assert_eq!(to_lower(to_upper(c)), i32::from(ch.to_ascii_lowercase() as u8));
            }
        }
    }
}
