//! The `printf` family's engine, written so it can be tested without a C compiler.
//!
//! `printf` is where a libc is usually least tested and most trusted: it is called
//! by every program, from error paths, with arguments the compiler has already
//! type-erased. So the engine here takes its output through a [`Sink`] and its
//! arguments through an [`Args`] source, and the host tests drive both with
//! ordinary Rust values. The variadic wrappers in `stdio` are then a thin layer
//! whose only job is to pull the right type out of a `VaList`.
//!
//! What is supported: `%%`, `%c`, `%s`, `%d`/`%i`, `%u`, `%x`/`%X`, `%o`, `%p`,
//! `%f`, with the `-`, `+`, ` `, `0` and `#` flags, a width, a precision, and the
//! `hh`, `h`, `l`, `ll`, `z`, `t` length modifiers. Width and precision may be `*`.
//!
//! What is not: `%e`, `%g`, `%a`, `%n`, and positional (`%1$s`) specifiers. An
//! unsupported conversion is copied to the output verbatim rather than skipped —
//! a program that hits one sees `%g` in its output instead of a plausible wrong
//! number, and the difference is an afternoon.

use core::ffi::{c_char, c_void};

/// Somewhere formatted bytes go. Implementors count everything and store what they
/// can — `snprintf` must return the length it *would* have written.
pub(crate) trait Sink {
    fn push(&mut self, byte: u8);
}

/// The next argument, by type. `printf` has no type information at run time, so
/// what is pulled out is decided entirely by the format string.
pub(crate) trait Args {
    fn int(&mut self, len: Length) -> i64;
    fn uint(&mut self, len: Length) -> u64;
    fn ptr(&mut self) -> *const c_void;
    fn double(&mut self) -> f64;
}

/// The length modifier of a conversion. It decides how wide the argument was
/// *promoted* to, which matters for the sign: `%hhd` of 200 is -56.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Length {
    Char,
    Short,
    Int,
    Long,
    LongLong,
    Size,
}

/// A sink that writes into a fixed buffer and counts everything.
pub(crate) struct BufSink<'a> {
    pub buf: &'a mut [u8],
    pub written: usize,
}

impl Sink for BufSink<'_> {
    fn push(&mut self, byte: u8) {
        if self.written < self.buf.len() {
            self.buf[self.written] = byte;
        }
        self.written += 1;
    }
}

/// Flags and sizes parsed out of one conversion.
#[derive(Default)]
struct Spec {
    left: bool,
    zero: bool,
    plus: bool,
    space: bool,
    alt: bool,
    width: usize,
    precision: Option<usize>,
    length: Option<Length>,
}

/// Format `fmt` into `sink`, pulling arguments from `args`. Returns the number of
/// bytes the complete output would have taken.
///
/// # Safety
/// `%s` and `%p` arguments are read as pointers; they must be valid C strings and
/// pointers respectively, as C requires of the caller.
pub(crate) unsafe fn format(sink: &mut dyn Sink, fmt: &[u8], args: &mut dyn Args) -> usize {
    let mut count = Counter { inner: sink, n: 0 };
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            count.push(fmt[i]);
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        if i >= fmt.len() {
            // A trailing '%' is not a conversion; print it rather than swallowing it.
            count.push(b'%');
            break;
        }
        if fmt[i] == b'%' {
            count.push(b'%');
            i += 1;
            continue;
        }

        let mut spec = Spec::default();
        // Flags.
        while i < fmt.len() {
            match fmt[i] {
                b'-' => spec.left = true,
                b'0' => spec.zero = true,
                b'+' => spec.plus = true,
                b' ' => spec.space = true,
                b'#' => spec.alt = true,
                _ => break,
            }
            i += 1;
        }
        // Width, possibly from an argument.
        if i < fmt.len() && fmt[i] == b'*' {
            let w = args.int(Length::Int);
            if w < 0 {
                spec.left = true;
                spec.width = w.unsigned_abs() as usize;
            } else {
                spec.width = w as usize;
            }
            i += 1;
        } else {
            while i < fmt.len() && fmt[i].is_ascii_digit() {
                spec.width = spec.width * 10 + usize::from(fmt[i] - b'0');
                i += 1;
            }
        }
        // Precision.
        if i < fmt.len() && fmt[i] == b'.' {
            i += 1;
            let mut p = 0;
            if i < fmt.len() && fmt[i] == b'*' {
                p = args.int(Length::Int).max(0) as usize;
                i += 1;
            } else {
                while i < fmt.len() && fmt[i].is_ascii_digit() {
                    p = p * 10 + usize::from(fmt[i] - b'0');
                    i += 1;
                }
            }
            spec.precision = Some(p);
        }
        // Length modifier.
        while i < fmt.len() {
            match fmt[i] {
                b'h' => {
                    spec.length = Some(match spec.length {
                        Some(Length::Short) => Length::Char,
                        _ => Length::Short,
                    })
                }
                b'l' => {
                    spec.length = Some(match spec.length {
                        Some(Length::Long) => Length::LongLong,
                        _ => Length::Long,
                    })
                }
                b'z' | b't' => spec.length = Some(Length::Size),
                b'j' => spec.length = Some(Length::LongLong),
                _ => break,
            }
            i += 1;
        }
        if i >= fmt.len() {
            break;
        }
        let conversion = fmt[i];
        i += 1;
        let len = spec.length.unwrap_or(Length::Int);

        match conversion {
            b'c' => {
                let c = args.int(Length::Int) as u8;
                pad(&mut count, &spec, 1, |s| s.push(c));
            }
            b's' => {
                let p = args.ptr().cast::<c_char>();
                // SAFETY: C's contract for `%s`; a null pointer is the one case
                // worth surviving, because printing "(null)" is how you find out
                // which call site did it.
                let bytes: &[u8] = if p.is_null() {
                    b"(null)"
                } else {
                    unsafe { crate::string::as_bytes(p) }
                };
                let n = spec.precision.map_or(bytes.len(), |p| p.min(bytes.len()));
                pad(&mut count, &spec, n, |s| {
                    for &b in &bytes[..n] {
                        s.push(b);
                    }
                });
            }
            b'd' | b'i' => {
                let v = args.int(len);
                let sign = if v < 0 {
                    Some(b'-')
                } else if spec.plus {
                    Some(b'+')
                } else if spec.space {
                    Some(b' ')
                } else {
                    None
                };
                integer(&mut count, &spec, v.unsigned_abs(), 10, false, sign, false);
            }
            b'u' => integer(&mut count, &spec, args.uint(len), 10, false, None, false),
            b'x' => integer(&mut count, &spec, args.uint(len), 16, false, None, spec.alt),
            b'X' => integer(&mut count, &spec, args.uint(len), 16, true, None, spec.alt),
            b'o' => integer(&mut count, &spec, args.uint(len), 8, false, None, spec.alt),
            b'p' => {
                let v = args.ptr() as usize as u64;
                let mut hex = spec;
                hex.alt = true;
                integer(&mut count, &hex, v, 16, false, None, true);
            }
            b'f' | b'F' => float(&mut count, &spec, args.double()),
            _ => {
                // Unsupported: emit the specifier as written, so it is visible in the
                // output instead of turning into a wrong-looking number.
                for &b in &fmt[start..i] {
                    count.push(b);
                }
            }
        }
    }
    count.n
}

/// Wraps a sink to count bytes, since `snprintf` returns what it *would* have
/// written and a truncating sink cannot say.
struct Counter<'a> {
    inner: &'a mut dyn Sink,
    n: usize,
}

impl Counter<'_> {
    fn push(&mut self, byte: u8) {
        self.inner.push(byte);
        self.n += 1;
    }
}

/// Emit `body` (which writes `len` bytes) inside the spec's width.
fn pad(out: &mut Counter, spec: &Spec, len: usize, body: impl FnOnce(&mut Counter)) {
    let fill = spec.width.saturating_sub(len);
    if spec.left {
        body(out);
        for _ in 0..fill {
            out.push(b' ');
        }
    } else {
        for _ in 0..fill {
            out.push(b' ');
        }
        body(out);
    }
}

/// Emit an integer with the spec's flags applied.
///
/// The interaction worth stating: `0` padding goes *after* the sign and any `0x`,
/// which is why this cannot be layered on top of [`pad`] — `-0042` is right and
/// `00-42` is what you get from padding the finished string.
fn integer(
    out: &mut Counter,
    spec: &Spec,
    value: u64,
    base: u64,
    upper: bool,
    sign: Option<u8>,
    prefix: bool,
) {
    let digits = b"0123456789abcdef";
    let upper_digits = b"0123456789ABCDEF";
    let table = if upper { upper_digits } else { digits };

    let mut buf = [0u8; 24];
    let mut n = 0;
    let mut v = value;
    loop {
        buf[n] = table[(v % base) as usize];
        v /= base;
        n += 1;
        if v == 0 {
            break;
        }
    }
    // A precision of 0 and a value of 0 print nothing at all, which C says and
    // which `%.0d` of 0 relies on.
    if spec.precision == Some(0) && value == 0 {
        n = 0;
    }
    let zeros = spec.precision.unwrap_or(0).saturating_sub(n);
    let prefix_len = if prefix && value != 0 {
        match base {
            16 => 2,
            8 => 1,
            _ => 0,
        }
    } else {
        0
    };
    let body = n + zeros + prefix_len + usize::from(sign.is_some());
    let fill = spec.width.saturating_sub(body);

    if !spec.left && !spec.zero {
        for _ in 0..fill {
            out.push(b' ');
        }
    }
    if let Some(s) = sign {
        out.push(s);
    }
    if prefix_len > 0 {
        out.push(b'0');
        if base == 16 {
            out.push(if upper { b'X' } else { b'x' });
        }
    }
    // Zero padding is only honoured when there is no explicit precision — C says a
    // precision overrides `0`, and glibc programs depend on `%08.3d` behaving.
    if !spec.left && spec.zero && spec.precision.is_none() {
        for _ in 0..fill {
            out.push(b'0');
        }
    }
    for _ in 0..zeros {
        out.push(b'0');
    }
    for i in (0..n).rev() {
        out.push(buf[i]);
    }
    if spec.left {
        for _ in 0..fill {
            out.push(b' ');
        }
    }
}

/// The largest magnitude `%f` renders digit-for-digit. Past this the fraction
/// carries no information a `u128` can hold, and printing more digits would be
/// inventing them.
const FLOAT_EXACT_LIMIT: f64 = 1e18;

/// Emit `%f`.
///
/// This is fixed-point rendering, not a shortest-round-trip algorithm: the value is
/// scaled by 10^precision in `u128` and rounded half-away-from-zero. For the range
/// a program actually prints — coordinates, sizes, times — it matches glibc. It is
/// *not* a `strtod`/`%.17g` round-trip, and `docs/LIBC-CONTRACT.md` says so rather
/// than leaving a caller to discover it.
fn float(out: &mut Counter, spec: &Spec, value: f64) {
    let precision = spec.precision.unwrap_or(6).min(17);
    let negative = value.is_sign_negative();
    let sign = if negative {
        Some(b'-')
    } else if spec.plus {
        Some(b'+')
    } else if spec.space {
        Some(b' ')
    } else {
        None
    };

    if value.is_nan() || value.is_infinite() {
        let text: &[u8] = if value.is_nan() { b"nan" } else { b"inf" };
        let len = text.len() + usize::from(sign.is_some() && !value.is_nan());
        pad(out, spec, len, |s| {
            if let Some(c) = sign {
                if !value.is_nan() {
                    s.push(c);
                }
            }
            for &b in text {
                s.push(b);
            }
        });
        return;
    }

    let magnitude = value.abs();
    if magnitude >= FLOAT_EXACT_LIMIT {
        // Too large for the fixed-point path: print the integer part as far as a
        // u128 goes and stop, rather than emitting digits that are noise.
        let whole = magnitude as u128;
        emit_fixed(out, spec, sign, whole, 0, 0);
        return;
    }

    let scale = 10u128.pow(precision as u32);
    // `f64::round` lives in `std`; this is the same thing for a non-negative value,
    // and `magnitude` is an absolute value by construction. The cast truncates, so
    // adding a half turns it into round-half-away-from-zero — which is what C's
    // `%f` does.
    let scaled = (magnitude * scale as f64 + 0.5) as u128;
    let whole = scaled / scale;
    let frac = scaled % scale;
    emit_fixed(out, spec, sign, whole, frac, precision);
}

/// Emit `sign whole [. frac]` inside the spec's width.
fn emit_fixed(
    out: &mut Counter,
    spec: &Spec,
    sign: Option<u8>,
    whole: u128,
    frac: u128,
    precision: usize,
) {
    let mut digits = [0u8; 40];
    let mut n = 0;
    let mut v = whole;
    loop {
        digits[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
        if v == 0 || n == digits.len() {
            break;
        }
    }
    let body = n + usize::from(sign.is_some()) + if precision > 0 { precision + 1 } else { 0 };
    let fill = spec.width.saturating_sub(body);

    if !spec.left && !spec.zero {
        for _ in 0..fill {
            out.push(b' ');
        }
    }
    if let Some(c) = sign {
        out.push(c);
    }
    if !spec.left && spec.zero {
        for _ in 0..fill {
            out.push(b'0');
        }
    }
    for i in (0..n).rev() {
        out.push(digits[i]);
    }
    if precision > 0 {
        out.push(b'.');
        let mut scale = 10u128.pow(precision as u32);
        let mut rest = frac;
        for _ in 0..precision {
            scale /= 10;
            out.push(b'0' + (rest / scale) as u8);
            rest %= scale;
        }
    }
    if spec.left {
        for _ in 0..fill {
            out.push(b' ');
        }
    }
}

/// Narrow a promoted argument to what the length modifier asked for, keeping the
/// sign. `%hhd` of 200 is -56, and a libc that prints 200 is wrong in a way nobody
/// notices until a checksum disagrees.
pub(crate) fn narrow_signed(value: i64, len: Length) -> i64 {
    match len {
        Length::Char => i64::from(value as i8),
        Length::Short => i64::from(value as i16),
        Length::Int => i64::from(value as i32),
        Length::Long | Length::LongLong | Length::Size => value,
    }
}

/// The unsigned counterpart of [`narrow_signed`].
pub(crate) fn narrow_unsigned(value: u64, len: Length) -> u64 {
    match len {
        Length::Char => u64::from(value as u8),
        Length::Short => u64::from(value as u16),
        Length::Int => u64::from(value as u32),
        Length::Long | Length::LongLong | Length::Size => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An argument source backed by a list of values — the host stand-in for a
    /// `VaList`.
    enum Arg {
        Int(i64),
        Uint(u64),
        Ptr(*const c_void),
        Double(f64),
    }

    struct Slice {
        args: alloc::vec::Vec<Arg>,
        at: usize,
    }

    extern crate alloc;

    impl Args for Slice {
        fn int(&mut self, len: Length) -> i64 {
            let v = match self.args[self.at] {
                Arg::Int(v) => v,
                Arg::Uint(v) => v as i64,
                _ => panic!("argument {} is not an integer", self.at),
            };
            self.at += 1;
            narrow_signed(v, len)
        }
        fn uint(&mut self, len: Length) -> u64 {
            let v = match self.args[self.at] {
                Arg::Uint(v) => v,
                Arg::Int(v) => v as u64,
                _ => panic!("argument {} is not an integer", self.at),
            };
            self.at += 1;
            narrow_unsigned(v, len)
        }
        fn ptr(&mut self) -> *const c_void {
            let v = match self.args[self.at] {
                Arg::Ptr(p) => p,
                _ => panic!("argument {} is not a pointer", self.at),
            };
            self.at += 1;
            v
        }
        fn double(&mut self) -> f64 {
            let v = match self.args[self.at] {
                Arg::Double(v) => v,
                _ => panic!("argument {} is not a double", self.at),
            };
            self.at += 1;
            v
        }
    }

    fn run(fmt: &str, args: alloc::vec::Vec<Arg>) -> (alloc::string::String, usize) {
        let mut buf = [0u8; 256];
        let n = {
            let mut sink = BufSink { buf: &mut buf, written: 0 };
            let mut source = Slice { args, at: 0 };
            // SAFETY: every pointer in the tests points at a NUL-terminated array
            // that outlives the call.
            unsafe { format(&mut sink, fmt.as_bytes(), &mut source) }
        };
        let text = alloc::string::String::from_utf8_lossy(&buf[..n.min(buf.len())]).into_owned();
        (text, n)
    }

    #[test]
    fn plain_text_and_percent() {
        assert_eq!(run("hello", alloc::vec![]).0, "hello");
        assert_eq!(run("100%%", alloc::vec![]).0, "100%");
        // A trailing '%' is not a conversion and must not eat the next argument.
        assert_eq!(run("done %", alloc::vec![]).0, "done %");
    }

    #[test]
    fn integers() {
        assert_eq!(run("%d", alloc::vec![Arg::Int(42)]).0, "42");
        assert_eq!(run("%d", alloc::vec![Arg::Int(-42)]).0, "-42");
        assert_eq!(run("%5d|", alloc::vec![Arg::Int(42)]).0, "   42|");
        assert_eq!(run("%-5d|", alloc::vec![Arg::Int(42)]).0, "42   |");
        assert_eq!(run("%05d", alloc::vec![Arg::Int(42)]).0, "00042");
        // Zero padding goes after the sign; padding a finished string gives "00-42".
        assert_eq!(run("%05d", alloc::vec![Arg::Int(-42)]).0, "-0042");
        assert_eq!(run("%+d", alloc::vec![Arg::Int(42)]).0, "+42");
        assert_eq!(run("%x", alloc::vec![Arg::Uint(255)]).0, "ff");
        assert_eq!(run("%X", alloc::vec![Arg::Uint(255)]).0, "FF");
        assert_eq!(run("%#x", alloc::vec![Arg::Uint(255)]).0, "0xff");
        assert_eq!(run("%o", alloc::vec![Arg::Uint(8)]).0, "10");
        assert_eq!(run("%.5d", alloc::vec![Arg::Int(42)]).0, "00042");
        assert_eq!(run("%.0d", alloc::vec![Arg::Int(0)]).0, "");
    }

    #[test]
    fn length_modifiers_narrow() {
        // The promotion rule: `%hhd` of 200 is -56. Ignoring the modifier prints
        // 200 and is wrong in a way that survives casual testing.
        assert_eq!(run("%hhd", alloc::vec![Arg::Int(200)]).0, "-56");
        assert_eq!(run("%hd", alloc::vec![Arg::Int(70000)]).0, "4464");
        assert_eq!(run("%lld", alloc::vec![Arg::Int(70000)]).0, "70000");
        assert_eq!(run("%zu", alloc::vec![Arg::Uint(u64::MAX)]).0, "18446744073709551615");
    }

    #[test]
    fn strings_and_pointers() {
        let text = b"initramfs\0";
        let p = text.as_ptr().cast::<c_void>();
        assert_eq!(run("%s", alloc::vec![Arg::Ptr(p)]).0, "initramfs");
        assert_eq!(run("%.4s", alloc::vec![Arg::Ptr(p)]).0, "init");
        assert_eq!(run("%12s|", alloc::vec![Arg::Ptr(p)]).0, "   initramfs|");
        // A null `%s` must not be a fault: printing "(null)" is how the call site
        // gets found.
        assert_eq!(run("%s", alloc::vec![Arg::Ptr(core::ptr::null())]).0, "(null)");
        assert_eq!(run("%p", alloc::vec![Arg::Ptr(0x8000_0000 as *const c_void)]).0, "0x80000000");
    }

    #[test]
    fn floats() {
        assert_eq!(run("%f", alloc::vec![Arg::Double(1.5)]).0, "1.500000");
        assert_eq!(run("%.2f", alloc::vec![Arg::Double(1.005)]).0, "1.00");
        assert_eq!(run("%.0f", alloc::vec![Arg::Double(2.5)]).0, "3");
        assert_eq!(run("%.3f", alloc::vec![Arg::Double(-0.0005)]).0, "-0.001");
        assert_eq!(run("%8.2f|", alloc::vec![Arg::Double(3.14159)]).0, "    3.14|");
        assert_eq!(run("%f", alloc::vec![Arg::Double(f64::INFINITY)]).0, "inf");
        assert_eq!(run("%f", alloc::vec![Arg::Double(f64::NAN)]).0, "nan");
    }

    #[test]
    fn unsupported_conversion_is_visible() {
        // Skipping it silently would produce output that looks right and is missing
        // a number; copying it through makes the gap obvious in the log.
        assert_eq!(run("%g", alloc::vec![]).0, "%g");
    }

    #[test]
    fn truncation_reports_the_full_length() {
        // `snprintf` returns what it *would* have written — callers size buffers
        // from that, so a truncating implementation that returns the truncated
        // length makes them allocate too little, forever.
        let mut buf = [0u8; 4];
        let n = {
            let mut sink = BufSink { buf: &mut buf, written: 0 };
            let mut source = Slice { args: alloc::vec![Arg::Int(123_456)], at: 0 };
            // SAFETY: no pointer arguments here.
            unsafe { format(&mut sink, b"%d", &mut source) }
        };
        assert_eq!(n, 6);
        assert_eq!(&buf, b"1234");
    }
}
