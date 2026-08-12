//! Reading numbers and text back out of strings: `strtod` and the `scanf` family.
//!
//! These are the mirror of `fmt.rs`, and the same split applies — the engine works
//! over slices and a source of destination pointers, the C entry points are compiled
//! only for the target. It is here because Qt parses: `QDoubleValidator`, style
//! sheets, `.desktop` files and every `QVariant` conversion from a string end up in
//! `strtod`, and the contract lists `__isoc23_sscanf` because glibc's headers rename
//! `sscanf` to it whenever a program is compiled for C23 or later.
//!
//! ## The accuracy of `strtod`, precisely
//! A decimal literal is exactly representable as a double only sometimes, so the
//! question is how far the answer can be from the nearest double:
//!
//! * Up to 19 significant digits with a decimal exponent in `-22..=22`, the answer
//!   is **correctly rounded** — the digits fit an integer exactly, the power of ten
//!   is exact, and one multiplication or division rounds once. `0.1` comes back as
//!   the same double the compiler produces for the literal `0.1`, and the test says
//!   so by comparing against the compiler's own.
//! * Outside it the scaling walks in steps of `10^22`, keeping the running value as
//!   an unevaluated pair so each step's rounding lands in a low word instead of
//!   being discarded. The test compares a few thousand generated literals against
//!   Rust's own correctly-rounded parser and the answer is within **one ulp**,
//!   usually equal.
//!
//! Being *always* equal needs big-integer arithmetic, which is what glibc and Rust
//! do and what is not here. A program printing a value with `%.17g` and reading it
//! back can therefore see the last bit move.

use core::ffi::{c_char, c_int, c_void};

use crate::math;

/// Powers of ten that are exact in a double: `10^22` is the last one whose value
/// has no rounding, because `5^23` needs 54 bits.
const POW10: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

/// Is this byte one of C's whitespace characters?
pub(crate) fn is_space(b: u8) -> bool {
    b == b' ' || (0x09..=0x0d).contains(&b)
}

/// `strtod`: the value, and how many bytes it consumed.
///
/// A consumed count of zero means "no number here", which is how a caller
/// distinguishes a literal `0` from a failure — the same convention
/// [`crate::string::strtol`] uses.
pub(crate) fn strtod(s: &[u8]) -> (f64, usize) {
    let mut i = 0;
    while i < s.len() && is_space(s[i]) {
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
    let sign = if negative { -1.0 } else { 1.0 };

    // `inf`, `infinity` and `nan`, which C requires and which a JSON or CSS parser
    // will eventually hand us.
    if let Some(n) = match_ci(&s[i..], b"infinity").or_else(|| match_ci(&s[i..], b"inf")) {
        return (sign * f64::INFINITY, i + n);
    }
    if let Some(n) = match_ci(&s[i..], b"nan") {
        return (f64::NAN, i + n);
    }

    // Mantissa digits, with the decimal point folded into the exponent rather than
    // handled separately: "1.25" and "125e-2" then take exactly the same path.
    let mut mantissa: u64 = 0;
    let mut digits = 0;
    let mut exponent: i32 = 0;
    let mut seen_point = false;
    let mut any_digit = false;
    while let Some(&c) = s.get(i) {
        match c {
            b'0'..=b'9' => {
                any_digit = true;
                if digits < 19 {
                    mantissa = mantissa * 10 + u64::from(c - b'0');
                    // A leading zero is not a significant digit; counting it would
                    // shorten the window in which this is correctly rounded.
                    if mantissa != 0 {
                        digits += 1;
                    }
                    if seen_point {
                        exponent -= 1;
                    }
                } else if !seen_point {
                    // Past the nineteenth digit the value cannot change; only the
                    // magnitude can.
                    exponent += 1;
                }
                i += 1;
            }
            b'.' if !seen_point => {
                seen_point = true;
                i += 1;
            }
            _ => break,
        }
    }
    if !any_digit {
        return (0.0, 0);
    }
    // The exponent, which only counts if it is well formed: "1e" is the number 1
    // followed by the letter e, and a parser that swallows the `e` corrupts the
    // caller's idea of where it stopped.
    if matches!(s.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        let negative_exp = match s.get(j) {
            Some(b'-') => {
                j += 1;
                true
            }
            Some(b'+') => {
                j += 1;
                false
            }
            _ => false,
        };
        let digits_start = j;
        let mut value: i32 = 0;
        while let Some(&c) = s.get(j) {
            if !c.is_ascii_digit() {
                break;
            }
            // Clamped: an exponent of 10^9 is going to overflow or flush to zero
            // whatever its exact value, and wrapping would turn it into a small one.
            value = (value * 10 + i32::from(c - b'0')).min(100_000);
            j += 1;
        }
        if j > digits_start {
            exponent += if negative_exp { -value } else { value };
            i = j;
        }
    }
    (sign * scale(mantissa, exponent), i)
}

/// `mantissa · 10^exponent`.
///
/// The easy case is one rounding and is exact in the sense that matters: `m` is an
/// integer under 2^53, `10^e` for `|e| ≤ 22` is exact, and IEEE multiplication and
/// division round once. That is the whole of what most literals need.
///
/// Outside it the power of ten is not representable, so the scaling walks there in
/// steps of at most `10^22` and keeps the running value as an unevaluated pair.
/// Each step's rounding lands in the low word instead of being thrown away, so
/// fifteen steps down to `10^-320` accumulate about `2^-102` of relative error —
/// three decimal orders below the last bit of the result.
///
/// The version before this one asked `pow` for `10^100` and multiplied. That is one
/// line shorter and lands ten ulp away, which the test caught.
fn scale(mantissa: u64, exponent: i32) -> f64 {
    if mantissa == 0 {
        return 0.0;
    }
    let m = mantissa as f64;
    if (-22..=22).contains(&exponent) && mantissa < (1u64 << 53) {
        return if exponent >= 0 {
            m * POW10[exponent as usize]
        } else {
            m / POW10[(-exponent) as usize]
        };
    }
    if exponent > 400 {
        return f64::INFINITY;
    }
    if exponent < -400 {
        return 0.0;
    }
    let mut v = (m, 0.0);
    let mut e = exponent;
    // A binary exponent held aside from the pair.
    //
    // Without it, `1.7976931348623157e308` — the largest double there is — comes
    // out NaN: the high word stays finite all the way, but the low word is `2^-53`
    // of it, so *its* last multiplication overflows to infinity and the
    // renormalisation adds an infinity to a finite number. Scaling by a power of
    // two is exact, so parking 256 binary orders here changes nothing except that
    // the arithmetic stays inside the exponent range.
    let mut binary = 0i32;
    while e > 0 {
        if math::fabs(v.0) > 1e250 {
            v = (math::ldexp(v.0, -256), math::ldexp(v.1, -256));
            binary += 256;
        }
        let k = e.min(22) as usize;
        v = dd_mul(v, POW10[k]);
        e -= k as i32;
    }
    while e < 0 {
        if math::fabs(v.0) < 1e-250 {
            v = (math::ldexp(v.0, 256), math::ldexp(v.1, 256));
            binary -= 256;
        }
        let k = (-e).min(22) as usize;
        v = dd_div(v, POW10[k]);
        e += k as i32;
    }
    // The pair is normalised, so its high word is already the rounded value.
    math::ldexp(v.0, binary)
}

/// An unevaluated pair times an exactly-representable double.
fn dd_mul(v: (f64, f64), c: f64) -> (f64, f64) {
    let (p, err) = math::two_prod(v.0, c);
    math::two_sum(p, v.1 * c + err)
}

/// An unevaluated pair divided by an exactly-representable double.
fn dd_div(v: (f64, f64), c: f64) -> (f64, f64) {
    let q = v.0 / c;
    // What the division actually threw away, recovered exactly and divided in turn.
    let (p, err) = math::two_prod(q, c);
    let r = (((v.0 - p) - err) + v.1) / c;
    math::two_sum(q, r)
}

/// Case-insensitive prefix match, returning the length matched.
fn match_ci(s: &[u8], word: &[u8]) -> Option<usize> {
    if s.len() < word.len() {
        return None;
    }
    for (a, b) in s.iter().zip(word) {
        if a.to_ascii_lowercase() != *b {
            return None;
        }
    }
    Some(word.len())
}

/// Where a conversion's result goes. One method, because every `scanf` argument is
/// a pointer — the format string is what decides how many bytes get written to it.
pub(crate) trait Dests {
    fn next(&mut self) -> *mut c_void;
}

/// The width of an integer destination, from the length modifier.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Width {
    Char,
    Short,
    Int,
    Long,
    LongLong,
    Size,
    Float,
    Double,
}

/// The value C's `scanf` returns when the input ended before any conversion.
const EOF: c_int = -1;

/// What to return when a conversion could not be made.
///
/// C draws a line here that is easy to miss and expensive to get wrong: running out
/// of *input* before assigning anything is `EOF`, while input that is present but
/// unmatchable is `0`. A program reading records in a `while (sscanf(...) != EOF)`
/// loop spins forever on the second case if both come back as `EOF`, and stops one
/// record early if both come back as `0`.
fn stop(assigned: c_int, pos: usize, len: usize) -> c_int {
    if assigned > 0 {
        assigned
    } else if pos >= len {
        EOF
    } else {
        0
    }
}

/// The `scanf` engine: match `fmt` against `input`, storing through `dests`.
///
/// Returns the number of *assignments* made — which is not the number of
/// conversions, because `%*d` matches and stores nothing, and not the number of
/// bytes consumed either. C programs branch on this number, so it is the one thing
/// here that must be exactly right.
///
/// # Safety
/// Every pointer `dests` yields must be valid for the type its conversion implies.
pub(crate) unsafe fn scan(input: &[u8], fmt: &[u8], dests: &mut dyn Dests) -> c_int {
    let mut pos = 0; // into `input`
    let mut f = 0; // into `fmt`
    let mut assigned = 0;
    let mut any_input_seen = false;

    while f < fmt.len() {
        let c = fmt[f];

        // Whitespace in the format matches any run of whitespace, including none.
        if is_space(c) {
            while pos < input.len() && is_space(input[pos]) {
                pos += 1;
            }
            f += 1;
            continue;
        }

        if c != b'%' {
            // A literal must match. C stops the whole call here on a mismatch.
            if pos >= input.len() {
                return stop(assigned, pos, input.len());
            }
            if input[pos] != c {
                return assigned;
            }
            pos += 1;
            f += 1;
            any_input_seen = true;
            continue;
        }

        f += 1;
        if f >= fmt.len() {
            break;
        }
        if fmt[f] == b'%' {
            if pos >= input.len() || input[pos] != b'%' {
                return stop(assigned, pos, input.len());
            }
            pos += 1;
            f += 1;
            continue;
        }

        // `*` suppresses the assignment but not the matching.
        let suppress = fmt[f] == b'*';
        if suppress {
            f += 1;
        }

        // Maximum field width.
        let mut max = usize::MAX;
        if fmt.get(f).is_some_and(u8::is_ascii_digit) {
            let mut w = 0usize;
            while let Some(&d) = fmt.get(f) {
                if !d.is_ascii_digit() {
                    break;
                }
                w = w * 10 + usize::from(d - b'0');
                f += 1;
            }
            max = w;
        }

        // Length modifier.
        let mut width = Width::Int;
        loop {
            match fmt.get(f) {
                Some(b'h') => {
                    width = if width == Width::Short { Width::Char } else { Width::Short };
                    f += 1;
                }
                Some(b'l') => {
                    width = if width == Width::Long { Width::LongLong } else { Width::Long };
                    f += 1;
                }
                Some(b'z') => {
                    width = Width::Size;
                    f += 1;
                }
                Some(b'j' | b't') => {
                    width = Width::LongLong;
                    f += 1;
                }
                Some(b'L') => {
                    width = Width::Double;
                    f += 1;
                }
                _ => break,
            }
        }

        let Some(&conv) = fmt.get(f) else { break };
        f += 1;

        // `%c` and `%[` take the input as it comes; everything else skips leading
        // whitespace first, which is what makes "%d %d" parse "3\n4".
        if !matches!(conv, b'c' | b'[' | b'n') {
            while pos < input.len() && is_space(input[pos]) {
                pos += 1;
            }
        }

        match conv {
            b'n' => {
                // Not a conversion: the count of bytes consumed so far. It does not
                // count toward the return value, which is a rule programs rely on.
                if !suppress {
                    // SAFETY: the caller's contract — the argument is an int*.
                    unsafe { store_int(dests.next(), pos as i64, width) };
                }
            }
            b'c' => {
                let n = if max == usize::MAX { 1 } else { max };
                if pos + n > input.len() {
                    return stop(assigned, pos, input.len());
                }
                if suppress {
                    pos += n;
                } else {
                    let dst = dests.next().cast::<u8>();
                    for k in 0..n {
                        // SAFETY: the caller promised room for `n` bytes; no NUL is
                        // written, because %c is not a string.
                        unsafe { *dst.add(k) = input[pos + k] };
                    }
                    pos += n;
                    assigned += 1;
                }
                any_input_seen = true;
            }
            b's' => {
                let start = pos;
                while pos < input.len() && !is_space(input[pos]) && pos - start < max {
                    pos += 1;
                }
                if pos == start {
                    return stop(assigned, pos, input.len());
                }
                if !suppress {
                    let dst = dests.next().cast::<u8>();
                    for (k, &b) in input[start..pos].iter().enumerate() {
                        // SAFETY: the caller's buffer, sized by the width they gave.
                        unsafe { *dst.add(k) = b };
                    }
                    // SAFETY: as above; %s always terminates.
                    unsafe { *dst.add(pos - start) = 0 };
                    assigned += 1;
                }
                any_input_seen = true;
            }
            b'[' => {
                let Some((set, negated, next_f)) = parse_set(fmt, f) else { break };
                f = next_f;
                let start = pos;
                while pos < input.len() && pos - start < max {
                    let inside = set_contains(&set, input[pos]);
                    if inside == negated {
                        break;
                    }
                    pos += 1;
                }
                if pos == start {
                    return stop(assigned, pos, input.len());
                }
                if !suppress {
                    let dst = dests.next().cast::<u8>();
                    for (k, &b) in input[start..pos].iter().enumerate() {
                        // SAFETY: as `%s`.
                        unsafe { *dst.add(k) = b };
                    }
                    // SAFETY: as above.
                    unsafe { *dst.add(pos - start) = 0 };
                    assigned += 1;
                }
                any_input_seen = true;
            }
            b'd' | b'i' | b'u' | b'o' | b'x' | b'X' | b'p' => {
                let base = match conv {
                    b'o' => 8,
                    b'x' | b'X' | b'p' => 16,
                    b'i' => 0,
                    _ => 10,
                };
                let slice = &input[pos..input.len().min(pos.saturating_add(max))];
                let (value, used) = crate::string::strtol(slice, base);
                if used == 0 {
                    return stop(assigned, pos, input.len());
                }
                pos += used;
                if !suppress {
                    let w = if conv == b'p' { Width::Long } else { width };
                    // SAFETY: the caller's contract.
                    unsafe { store_int(dests.next(), value, w) };
                    assigned += 1;
                }
                any_input_seen = true;
            }
            b'f' | b'e' | b'E' | b'g' | b'G' | b'a' => {
                let slice = &input[pos..input.len().min(pos.saturating_add(max))];
                let (value, used) = strtod(slice);
                if used == 0 {
                    return stop(assigned, pos, input.len());
                }
                pos += used;
                if !suppress {
                    let dst = dests.next();
                    // SAFETY: the caller's contract — `%f` wants a float*, `%lf` a
                    // double*. Getting this backwards writes four bytes of a double
                    // into a float, which is why the modifier is tracked at all.
                    unsafe {
                        if matches!(width, Width::Long | Width::LongLong | Width::Double) {
                            *dst.cast::<f64>() = value;
                        } else {
                            *dst.cast::<f32>() = value as f32;
                        }
                    }
                    assigned += 1;
                }
                any_input_seen = true;
            }
            _ => break, // an unknown conversion ends the call, as C says
        }
    }

    if assigned == 0 && !any_input_seen && pos >= input.len() {
        return EOF;
    }
    assigned
}

/// The 256-bit membership set behind `%[...]`, and where the format continues.
fn parse_set(fmt: &[u8], mut f: usize) -> Option<([u64; 4], bool, usize)> {
    let mut set = [0u64; 4];
    let negated = fmt.get(f) == Some(&b'^');
    if negated {
        f += 1;
    }
    // A `]` first is a literal `]`, not the end of the set.
    let mut first = true;
    loop {
        let &c = fmt.get(f)?;
        if c == b']' && !first {
            return Some((set, negated, f + 1));
        }
        first = false;
        // A range, unless the dash is last.
        if fmt.get(f + 1) == Some(&b'-') && fmt.get(f + 2).is_some_and(|&e| e != b']') {
            let end = fmt[f + 2];
            for b in c..=end {
                set[usize::from(b >> 6)] |= 1 << (b & 63);
            }
            f += 3;
        } else {
            set[usize::from(c >> 6)] |= 1 << (c & 63);
            f += 1;
        }
    }
}

fn set_contains(set: &[u64; 4], b: u8) -> bool {
    set[usize::from(b >> 6)] & (1 << (b & 63)) != 0
}

/// Store an integer through a `void*` at the width the length modifier asked for.
///
/// # Safety
/// `dst` must be valid for that many bytes.
unsafe fn store_int(dst: *mut c_void, value: i64, width: Width) {
    if dst.is_null() {
        return;
    }
    // SAFETY: the caller's contract; the width is what the format string promised.
    unsafe {
        match width {
            Width::Char => *dst.cast::<i8>() = value as i8,
            Width::Short => *dst.cast::<i16>() = value as i16,
            Width::Int | Width::Float => *dst.cast::<i32>() = value as i32,
            Width::Long | Width::LongLong | Width::Size | Width::Double => {
                *dst.cast::<i64>() = value;
            }
        }
    }
}

/// The C entry points.
#[cfg(not(test))]
pub mod exports {
    use super::{c_char, c_int, c_void, Dests};

    /// Destinations pulled from a `VaList`.
    struct VaDests<'a>(core::ffi::VaList<'a>);

    impl Dests for VaDests<'_> {
        fn next(&mut self) -> *mut c_void {
            // SAFETY: the format string promised one pointer per conversion; that is
            // the contract every `scanf` in C relies on.
            unsafe { self.0.next_arg::<*mut c_void>() }
        }
    }

    /// # Safety
    /// C ABI: `s` is NUL-terminated; `end` is null or a writable pointer.
    #[no_mangle]
    pub unsafe extern "C" fn strtod(s: *const c_char, end: *mut *mut c_char) -> f64 {
        // SAFETY: forwarded from the caller.
        unsafe {
            let bytes = crate::string::as_bytes(s);
            let (value, used) = super::strtod(bytes);
            if !end.is_null() {
                *end = s.add(used).cast_mut();
            }
            value
        }
    }

    /// # Safety
    /// As [`strtod`].
    #[no_mangle]
    pub unsafe extern "C" fn strtof(s: *const c_char, end: *mut *mut c_char) -> f32 {
        // SAFETY: forwarded from the caller.
        unsafe { strtod(s, end) as f32 }
    }

    /// # Safety
    /// As [`strtod`]. `long double` is 128-bit on this target and this returns a
    /// `double`'s worth of precision in it — the alternative is failing to link
    /// libstdc++, which uses this name in `std::stold`.
    #[no_mangle]
    pub unsafe extern "C" fn strtold(s: *const c_char, end: *mut *mut c_char) -> f64 {
        // SAFETY: forwarded from the caller.
        unsafe { strtod(s, end) }
    }

    /// # Safety
    /// C ABI: NUL-terminated string.
    #[no_mangle]
    pub unsafe extern "C" fn atof(s: *const c_char) -> f64 {
        // SAFETY: forwarded from the caller.
        unsafe { super::strtod(crate::string::as_bytes(s)).0 }
    }

    /// # Safety
    /// C ABI: `input` and `format` are NUL-terminated; the variadic arguments are
    /// pointers matching the conversions.
    #[no_mangle]
    pub unsafe extern "C" fn sscanf(
        input: *const c_char,
        format: *const c_char,
        args: ...
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe {
            let text = crate::string::as_bytes(input);
            let fmt = crate::string::as_bytes(format);
            super::scan(text, fmt, &mut VaDests(args))
        }
    }

    /// The name glibc's headers rewrite `sscanf` to when a program is compiled as
    /// C23, which is why it is in the contract: Qt's build does that, and a library
    /// without this symbol fails to link a translation unit whose source says
    /// `sscanf`. The difference in C23 is that `%b` exists; everything else is the
    /// same function.
    ///
    /// # Safety
    /// As [`sscanf`].
    #[no_mangle]
    pub unsafe extern "C" fn __isoc23_sscanf(
        input: *const c_char,
        format: *const c_char,
        args: ...
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe {
            let text = crate::string::as_bytes(input);
            let fmt = crate::string::as_bytes(format);
            super::scan(text, fmt, &mut VaDests(args))
        }
    }

    /// # Safety
    /// As [`sscanf`], with the list already started by the caller.
    #[no_mangle]
    pub unsafe extern "C" fn vsscanf(
        input: *const c_char,
        format: *const c_char,
        args: core::ffi::VaList,
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe {
            let text = crate::string::as_bytes(input);
            let fmt = crate::string::as_bytes(format);
            super::scan(text, fmt, &mut VaDests(args))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> (f64, usize) {
        strtod(s.as_bytes())
    }

    #[test]
    fn strtod_is_correctly_rounded_in_its_window() {
        // The claim: identical to the double the *compiler* produced for the same
        // literal. Not "close to" — the same bits.
        let cases: &[(&str, f64)] = &[
            ("0", 0.0),
            ("1", 1.0),
            ("0.1", 0.1),
            ("0.2", 0.2),
            ("3.14159265358979", 3.14159265358979),
            ("-2.5", -2.5),
            ("+7", 7.0),
            ("1e10", 1e10),
            ("1E-10", 1e-10),
            ("1.5e3", 1.5e3),
            ("123456789.123456", 123456789.123456),
            ("0.000001", 0.000001),
            ("9007199254740993", 9007199254740993.0),
            ("1e22", 1e22),
            ("2.2250738585072014e-22", 2.2250738585072014e-22),
        ];
        for &(text, want) in cases {
            let (got, used) = parse(text);
            assert_eq!(got.to_bits(), want.to_bits(), "strtod({text}) = {got}, want {want}");
            assert_eq!(used, text.len(), "strtod({text}) consumed {used}");
        }
    }

    #[test]
    fn strtod_outside_the_window_is_within_one_ulp() {
        let cases: &[(&str, f64)] = &[
            ("1e100", 1e100),
            ("1e-100", 1e-100),
            ("1.7976931348623157e308", 1.7976931348623157e308),
            ("4.9e-30", 4.9e-30),
            ("123456789012345678901234567890", 123456789012345678901234567890.0),
        ];
        for &(text, want) in cases {
            let (got, _) = parse(text);
            let ulps = ((got.to_bits() as i64) - (want.to_bits() as i64)).abs();
            assert!(ulps <= 1, "strtod({text}) is {ulps} ulp from {want}");
        }
        assert_eq!(parse("1e400").0, f64::INFINITY);
        assert_eq!(parse("1e-400").0, 0.0);
    }

    /// The broad claim, against a reference that is correctly rounded by
    /// construction: Rust's own `str::parse`. A few thousand literals spanning the
    /// exponent range, with mantissas of every length up to nineteen digits.
    #[test]
    fn strtod_matches_a_correctly_rounded_parser() {
        let mut state = 0x2545_F491_4F6C_DD1Du64; // xorshift, so the sweep is reproducible
        let mut equal = 0;
        let mut total = 0;
        let mut worst = 0i64;
        for _ in 0..4000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let digits = 1 + (state % 19) as u32;
            let mantissa = state % 10u64.pow(digits);
            let exponent = (state >> 32) as i32 % 300;
            let text = format!("{mantissa}e{exponent}");
            let want: f64 = text.parse().unwrap();
            let (got, used) = parse(&text);
            assert_eq!(used, text.len(), "strtod({text}) stopped early");
            let ulps = ((got.to_bits() as i64) - (want.to_bits() as i64)).abs();
            assert!(ulps <= 1, "strtod({text}) = {got}, want {want}, {ulps} ulp");
            worst = worst.max(ulps);
            total += 1;
            equal += i32::from(ulps == 0);
        }
        // Not an accuracy claim — a statement of what the sweep found, so that a
        // change making it dramatically worse shows up here rather than silently.
        assert!(equal * 100 / total >= 95, "only {equal} of {total} were exact");
        assert_eq!(worst, 1);
    }

    #[test]
    fn strtod_reports_where_it_stopped() {
        // The consumed count is the contract with `endptr`, and every one of these
        // is a case where a sloppy parser swallows one byte too many.
        assert_eq!(parse("  12abc"), (12.0, 4));
        assert_eq!(parse("1e"), (1.0, 1), "a bare exponent marker is not consumed");
        assert_eq!(parse("1e+"), (1.0, 1));
        assert_eq!(parse("1.5.5"), (1.5, 3), "the second point ends the number");
        assert_eq!(parse("."), (0.0, 0), "a lone point is not a number");
        assert_eq!(parse("abc"), (0.0, 0));
        assert_eq!(parse(""), (0.0, 0));
        assert_eq!(parse("-.5"), (-0.5, 3));
        assert_eq!(parse("infinity!"), (f64::INFINITY, 8));
        assert_eq!(parse("INF").0, f64::INFINITY);
        assert!(parse("nan").0.is_nan());
        assert_eq!(parse("-inf").0, f64::NEG_INFINITY);
    }

    /// Destinations backed by a fixed array of pointers the test filled in.
    struct Slots<'a> {
        slots: &'a [*mut c_void],
        next: usize,
    }

    impl Dests for Slots<'_> {
        fn next(&mut self) -> *mut c_void {
            let p = self.slots[self.next];
            self.next += 1;
            p
        }
    }

    fn run(input: &str, fmt: &str, slots: &[*mut c_void]) -> c_int {
        let mut d = Slots { slots, next: 0 };
        // SAFETY: every pointer in `slots` points at a local of the matching type.
        unsafe { scan(input.as_bytes(), fmt.as_bytes(), &mut d) }
    }

    #[test]
    fn sscanf_integers_and_whitespace() {
        let (mut a, mut b, mut c) = (0i32, 0i32, 0i32);
        let slots = [
            core::ptr::addr_of_mut!(a).cast::<c_void>(),
            core::ptr::addr_of_mut!(b).cast::<c_void>(),
            core::ptr::addr_of_mut!(c).cast::<c_void>(),
        ];
        assert_eq!(run("3 \n\t 4 -5", "%d %d %d", &slots), 3);
        assert_eq!((a, b, c), (3, 4, -5));

        // A literal in the format must match, and the count reflects how far it got.
        let mut x = 0i32;
        let one = [core::ptr::addr_of_mut!(x).cast::<c_void>()];
        assert_eq!(run("42px", "%dpx", &one), 1);
        assert_eq!(x, 42);
        assert_eq!(run("42em", "%dpx", &one), 1, "the literal failed after one assignment");

        // Bases.
        let mut h = 0i32;
        let hs = [core::ptr::addr_of_mut!(h).cast::<c_void>()];
        assert_eq!(run("ff", "%x", &hs), 1);
        assert_eq!(h, 255);
        assert_eq!(run("0x1f", "%i", &hs), 1);
        assert_eq!(h, 31);
        assert_eq!(run("777", "%o", &hs), 1);
        assert_eq!(h, 511);
    }

    #[test]
    fn sscanf_widths_suppression_and_lengths() {
        let (mut a, mut b) = (0i32, 0i32);
        let slots = [
            core::ptr::addr_of_mut!(a).cast::<c_void>(),
            core::ptr::addr_of_mut!(b).cast::<c_void>(),
        ];
        // A width splits a run of digits that no separator splits.
        assert_eq!(run("20260813", "%4d%2d", &slots), 2);
        assert_eq!((a, b), (2026, 8));

        // `*` matches without assigning, so the return value counts one.
        let one = [core::ptr::addr_of_mut!(a).cast::<c_void>()];
        a = 0;
        assert_eq!(run("7 9", "%*d %d", &one), 1);
        assert_eq!(a, 9);

        // The length modifier decides how many bytes are written. Writing eight
        // where the caller has two is how a scanf corrupts the stack.
        let mut small = [0u8; 8];
        small[2] = 0xAA;
        let s = [small.as_mut_ptr().cast::<c_void>()];
        assert_eq!(run("-1", "%hd", &s), 1);
        assert_eq!(small[0..2], [0xff, 0xff], "%hd wrote two bytes");
        assert_eq!(small[2], 0xAA, "%hd left the third byte alone");
    }

    #[test]
    fn sscanf_strings_chars_and_sets() {
        let mut buf = [0u8; 32];
        let one = [buf.as_mut_ptr().cast::<c_void>()];
        assert_eq!(run("  hello world", "%s", &one), 1);
        assert_eq!(&buf[..6], b"hello\0", "%s stops at whitespace and terminates");

        // %c takes the next byte whatever it is — including a space, which is the
        // whole difference from %1s.
        let mut ch = [0u8; 2];
        let cs = [ch.as_mut_ptr().cast::<c_void>()];
        assert_eq!(run(" x", "%c", &cs), 1);
        assert_eq!(ch[0], b' ');

        // Scan sets, both senses, with a range.
        buf = [0u8; 32];
        assert_eq!(run("abc123", "%[a-z]", &one), 1);
        assert_eq!(&buf[..4], b"abc\0");
        buf = [0u8; 32];
        assert_eq!(run("key=value", "%[^=]", &one), 1);
        assert_eq!(&buf[..4], b"key\0");
    }

    #[test]
    fn sscanf_floats_and_the_byte_count() {
        let mut d = 0f64;
        let mut fl = 0f32;
        let mut n = 0i32;
        let slots = [
            core::ptr::addr_of_mut!(d).cast::<c_void>(),
            core::ptr::addr_of_mut!(fl).cast::<c_void>(),
            core::ptr::addr_of_mut!(n).cast::<c_void>(),
        ];
        assert_eq!(run("1.5 -2.25e2 rest", "%lf %f %n", &slots), 2, "%n does not count");
        assert_eq!(d, 1.5);
        assert_eq!(fl, -225.0);
        // Twelve, not eleven: the space in the format before `%n` matched the space
        // in the input, and `%n` reports everything consumed up to that point.
        assert_eq!(n, 12, "%n reported the bytes consumed");
    }

    #[test]
    fn sscanf_reports_failure_the_way_c_does() {
        let mut a = 0i32;
        let one = [core::ptr::addr_of_mut!(a).cast::<c_void>()];
        // Nothing matched and the input ran out: EOF, not zero. Programs loop on
        // `!= EOF` and spin forever if this returns 0.
        assert_eq!(run("", "%d", &one), -1);
        // Input present but unmatchable: zero conversions.
        assert_eq!(run("abc", "%d", &one), 0);
        assert_eq!(a, 0, "a failed conversion assigns nothing");
    }
}
