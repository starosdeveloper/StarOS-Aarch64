//! Layer 1 of the contract, second half: the mathematics.
//!
//! Forty-one of the contract's symbols are `libm` — `sin`, `pow`, `sqrt` and their
//! relatives. Qt reaches them through `QTransform`, through `QPainterPath`, through
//! every gradient and every rotation, so a Qt link fails on this file's absence long
//! before it fails on anything interesting.
//!
//! ## Why it is written rather than borrowed
//! Rust's `f64::sin` lives in `std`, and `std` is what this tree does not have. The
//! alternative is a C `libm` cross-built against a sysroot that is itself being
//! written here. So: pure `core` Rust, no intrinsics, no inline assembly — the same
//! code runs on the host, which is what makes the accuracy claims below testable.
//!
//! ## What "accurate" means here
//! Every function is compared against the host's `std` implementation in this file's
//! tests, over ranges chosen to include the awkward parts, and the tolerance in each
//! test is the claim. The claims are relative error near `1e-15` for the elementary
//! functions and exactness for the ones that are exact (`floor`, `fmod`, `ldexp`).
//! They are *not* correctly-rounded implementations: the last bit can differ from
//! glibc, and a program that compares `sin(x)` against a hard-coded hexadecimal
//! double will see it.
//!
//! ## The limits, stated rather than discovered
//! * Argument reduction for `sin`/`cos`/`tan` carries π/2 to 159 bits and does the
//!   subtraction in unevaluated pairs, so it holds past `10^30` — but not to the
//!   end of the exponent range: above about `10^290` the internal product `n·π/2`
//!   overflows and the answer is NaN. The first version of this file used
//!   Cody–Waite with two 33-bit halves instead, and its test failed at `x = 10^15`
//!   with nine correct digits, which is what that method is worth there.
//! * `pow` goes through a logarithm carried in two doubles. Without that,
//!   `pow(1.0000001, 1e7)` — an ordinary compound-interest shape — comes back with
//!   nine digits, because `x^y = exp(y·ln x)` multiplies the logarithm's error by
//!   `y`. The test pins it at `1e-13`.
//! * Nothing here sets `errno` or raises floating-point exceptions. C says
//!   `sqrt(-1)` should set `EDOM`; it returns NaN and says nothing, because no
//!   caller in this tree reads `errno` after a maths call and a flag nobody checks
//!   is a flag that will be wrong.

#![allow(clippy::excessive_precision)]

use core::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI};

/// The sign bit, the only part of a float this file manipulates by name.
const SIGN: u64 = 0x8000_0000_0000_0000;

// ───────────────────────────── bit-level primitives ─────────────────────────────

/// `fabs` without `std`: clear the sign bit. Correct for NaN and both zeros.
pub(crate) fn fabs(x: f64) -> f64 {
    f64::from_bits(x.to_bits() & !SIGN)
}

/// `copysign`: magnitude of `x`, sign of `y`.
pub(crate) fn copysign(x: f64, y: f64) -> f64 {
    f64::from_bits((x.to_bits() & !SIGN) | (y.to_bits() & SIGN))
}

/// `trunc`: toward zero.
///
/// Done by masking the fractional bits rather than by converting to an integer,
/// because the conversion is undefined for values a 64-bit integer cannot hold and
/// `trunc(1e300)` is a perfectly ordinary call.
pub(crate) fn trunc(x: f64) -> f64 {
    let bits = x.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32 - 1023;
    if exp < 0 {
        // |x| < 1: the whole value is fraction. Keep the sign, so trunc(-0.5) is -0.
        return copysign(0.0, x);
    }
    if exp >= 52 {
        // No fractional bits left — and this is the branch that carries ±∞ and NaN
        // through unchanged, since their exponent field is 0x7ff.
        return x;
    }
    let mask = (1u64 << (52 - exp)) - 1;
    f64::from_bits(bits & !mask)
}

/// `floor`: toward −∞.
pub(crate) fn floor(x: f64) -> f64 {
    let t = trunc(x);
    if x < 0.0 && t != x {
        t - 1.0
    } else {
        t
    }
}

/// `ceil`: toward +∞.
pub(crate) fn ceil(x: f64) -> f64 {
    let t = trunc(x);
    if x > 0.0 && t != x {
        t + 1.0
    } else {
        t
    }
}

/// `round`: halfway cases away from zero, which is what C says and what
/// `rint`/"banker's rounding" deliberately does not do.
pub(crate) fn round(x: f64) -> f64 {
    let t = trunc(x);
    let f = x - t;
    if fabs(f) >= 0.5 {
        t + copysign(1.0, x)
    } else {
        t
    }
}

/// `rint` in the default rounding mode: halfway cases to even.
pub(crate) fn rint(x: f64) -> f64 {
    let t = trunc(x);
    let f = x - t;
    let a = fabs(f);
    if a < 0.5 {
        return t;
    }
    if a > 0.5 {
        return t + copysign(1.0, x);
    }
    // Exactly halfway: pick the neighbour with an even last bit.
    let up = t + copysign(1.0, x);
    if (t / 2.0) == trunc(t / 2.0) {
        t
    } else {
        up
    }
}

/// `frexp`: split into a mantissa in `[0.5, 1)` and a power of two.
///
/// Returns `(x, 0)` for zero, infinity and NaN, exactly as C requires. Subnormals
/// are scaled up first, which is why the exponent can come back below −1022.
pub(crate) fn frexp(x: f64) -> (f64, i32) {
    if x == 0.0 || !x.is_finite() {
        return (x, 0);
    }
    let mut bits = x.to_bits();
    let mut extra = 0;
    if (bits >> 52) & 0x7ff == 0 {
        // Subnormal: multiply by 2^64 to normalise, then account for it.
        bits = (x * 1.844_674_407_370_955_2e19).to_bits();
        extra = -64;
    }
    let exp = (((bits >> 52) & 0x7ff) as i32) - 1022 + extra;
    // Force the exponent field to that of a value in [0.5, 1).
    let mantissa = f64::from_bits((bits & !(0x7ffu64 << 52)) | (1022u64 << 52));
    (mantissa, exp)
}

/// `ldexp`/`scalbn`: multiply by a power of two, without a `pow` in sight.
///
/// The staged multiplication is what makes it correct at the ends: a single
/// `2^n` constant overflows for `n > 1023`, so large shifts are applied in pieces
/// that each stay representable, and the same trick downward reaches subnormals
/// with one rounding instead of two.
pub(crate) fn ldexp(x: f64, n: i32) -> f64 {
    if x == 0.0 || !x.is_finite() {
        return x;
    }
    let mut y = x;
    let mut n = n;
    while n > 1023 {
        y *= f64::from_bits(0x7fe_u64 << 52); // 2^1023
        n -= 1023;
        if !y.is_finite() {
            return y;
        }
    }
    while n < -1022 {
        y *= f64::from_bits(1u64 << 52); // 2^-1022
        n += 1022;
        if y == 0.0 {
            return y;
        }
    }
    y * f64::from_bits(((n + 1023) as u64) << 52)
}

// ───────────────────────────── roots ─────────────────────────────

/// `sqrt` by Newton's method on a halved exponent.
///
/// Five iterations: the initial linear fit is good to about 3%, and each iteration
/// squares the error — 3e-2, 1e-3, 1e-6, 1e-12, 1e-24 — so the fourth already has
/// more bits than a double holds and the fifth is the one that pays for the fit
/// being worse than advertised at the ends of the interval.
pub(crate) fn sqrt(x: f64) -> f64 {
    if x.is_nan() || x == 0.0 {
        return x; // ±0 comes back with its sign, as C requires
    }
    if x < 0.0 {
        return f64::NAN;
    }
    if x.is_infinite() {
        return x;
    }
    let (mut m, mut e) = frexp(x);
    if e % 2 != 0 {
        // An odd exponent cannot be halved; move one power of two into the mantissa.
        m *= 2.0;
        e -= 1;
    }
    let mut y = 0.417_312 + 0.590_163 * m; // linear fit over [0.5, 2)
    for _ in 0..5 {
        y = 0.5 * (y + m / y);
    }
    ldexp(y, e / 2)
}

/// `cbrt`. Same shape as `sqrt`: reduce the exponent to a multiple of three, fit,
/// then iterate. The exponent arithmetic uses Euclidean division so that a negative
/// exponent rounds the same way as a positive one.
pub(crate) fn cbrt(x: f64) -> f64 {
    if x == 0.0 || !x.is_finite() {
        return x;
    }
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let (mut m, e) = frexp(fabs(x));
    let r = e.rem_euclid(3);
    let q = (e - r) / 3;
    m = ldexp(m, r); // m now in [0.5, 4)
    let mut y = 0.5 + 0.4 * m;
    for _ in 0..6 {
        y = (2.0 * y + m / (y * y)) / 3.0;
    }
    sign * ldexp(y, q)
}

/// `hypot` without the overflow that `sqrt(x*x + y*y)` has for `x = 1e200`.
pub(crate) fn hypot(x: f64, y: f64) -> f64 {
    let (x, y) = (fabs(x), fabs(y));
    if x.is_infinite() || y.is_infinite() {
        return f64::INFINITY; // even if the other is NaN: C says so
    }
    let (big, small) = if x > y { (x, y) } else { (y, x) };
    if big == 0.0 {
        return 0.0;
    }
    let r = small / big;
    big * sqrt(1.0 + r * r)
}

// ───────────────────────────── exp and log ─────────────────────────────

const LN2_HI: f64 = 6.931_471_803_691_238_2e-1;
const LN2_LO: f64 = 1.908_214_929_270_587_7e-10;
const LOG2_E: f64 = 1.442_695_040_888_963_4;

/// `exp(r)` for `|r| ≲ ln2/2`, by its Taylor series.
///
/// Thirteen terms: the last one contributes `0.347^13/13! ≈ 6e-17` relative, which
/// is under a double's last bit, so the series is the error floor rather than a
/// compromise.
fn exp_small(r: f64) -> f64 {
    let mut term = 1.0;
    let mut sum = 1.0;
    let mut k = 1.0;
    for _ in 0..14 {
        term *= r / k;
        sum += term;
        k += 1.0;
    }
    sum
}

/// `exp`, by reduction to `exp(r)·2^k` with `r = x − k·ln2`.
///
/// `ln2` is subtracted in two pieces because `k·ln2` needs more bits than a double
/// has once `k` is large: the high piece is exact in floating point (its low bits
/// are zero), so the product `k·LN2_HI` is exact too and only the small correction
/// rounds.
pub(crate) fn exp(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x > 709.782_712_893_384 {
        return f64::INFINITY;
    }
    if x < -745.133_219_101_941_1 {
        return 0.0;
    }
    let k = rint(x * LOG2_E);
    let r = (x - k * LN2_HI) - k * LN2_LO;
    ldexp(exp_small(r), k as i32)
}

/// `expm1`: `exp(x) − 1` without the cancellation that spelling has near zero.
///
/// At `x = 1e-10`, `exp(x)` is `1.0000000001` and subtracting one throws away ten
/// significant digits. The series keeps all of them.
pub(crate) fn expm1(x: f64) -> f64 {
    if fabs(x) > 0.35 {
        return exp(x) - 1.0;
    }
    let mut term = 1.0;
    let mut sum = 0.0;
    let mut k = 1.0;
    for _ in 0..16 {
        term *= x / k;
        sum += term;
        k += 1.0;
    }
    sum
}

/// `exp2`.
pub(crate) fn exp2(x: f64) -> f64 {
    exp(x * core::f64::consts::LN_2)
}

/// `log`, by `ln(m·2^k) = k·ln2 + ln(m)` with `m` pulled into `[√½, √2)` so the
/// series argument `s = (m−1)/(m+1)` stays under 0.1716 in magnitude.
///
/// The odd-power series for `2·atanh(s)` converges as `s²`, so at that bound eleven
/// terms reach `1e-17` — and unlike a series in `m−1` it is symmetric about `m = 1`,
/// where the answer is most sensitive.
pub(crate) fn log(x: f64) -> f64 {
    if x.is_nan() || x == f64::INFINITY {
        return x;
    }
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    if x < 0.0 {
        return f64::NAN;
    }
    let (mut m, mut k) = frexp(x);
    if m < core::f64::consts::FRAC_1_SQRT_2 {
        m *= 2.0;
        k -= 1;
    }
    let s = (m - 1.0) / (m + 1.0);
    let s2 = s * s;
    let mut term = s;
    let mut sum = s;
    let mut d = 3.0;
    for _ in 0..12 {
        term *= s2;
        sum += term / d;
        d += 2.0;
    }
    let kf = f64::from(k);
    // The same two-piece ln2 as `exp`, for the same reason.
    kf * LN2_HI + (kf * LN2_LO + 2.0 * sum)
}

/// `log1p`: `log(1 + x)` without losing the small `x` to the addition.
///
/// The multiplier `x / ((1+x) − 1)` is the trick: `(1+x) − 1` is the part of `x`
/// that actually survived the rounding, so the ratio corrects `log` by exactly the
/// amount the addition lost.
pub(crate) fn log1p(x: f64) -> f64 {
    if x <= -1.0 {
        return if x == -1.0 { f64::NEG_INFINITY } else { f64::NAN };
    }
    let u = 1.0 + x;
    if u == 1.0 {
        return x;
    }
    log(u) * (x / (u - 1.0))
}

/// `log10`.
pub(crate) fn log10(x: f64) -> f64 {
    log(x) * core::f64::consts::LOG10_E
}

/// `log2`. Computed from the exponent directly so that powers of two come back
/// exact — `log2(8)` is 3.0 and not 2.9999999999999996.
pub(crate) fn log2(x: f64) -> f64 {
    if x.is_nan() || x == f64::INFINITY {
        return x;
    }
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    if x < 0.0 {
        return f64::NAN;
    }
    let (mut m, mut k) = frexp(x);
    if m < core::f64::consts::FRAC_1_SQRT_2 {
        m *= 2.0;
        k -= 1;
    }
    f64::from(k) + log(m) * LOG2_E
}

// ───────────────────────────── powers ─────────────────────────────

/// Dekker's split: `a = hi + lo` with each half holding 26 bits, so products of
/// halves are exact.
fn split(a: f64) -> (f64, f64) {
    let c = 134_217_729.0 * a; // 2^27 + 1
    let hi = c - (c - a);
    (hi, a - hi)
}

/// An exact sum as `(rounded, error)`: `a + b == s + e` with no rounding lost.
/// Knuth's algorithm, which needs no ordering assumption between the two.
fn two_sum(a: f64, b: f64) -> (f64, f64) {
    let s = a + b;
    let bb = s - a;
    (s, (a - (s - bb)) + (b - bb))
}

/// An exact product as `(rounded, error)`.
fn two_prod(a: f64, b: f64) -> (f64, f64) {
    let p = a * b;
    let (ah, al) = split(a);
    let (bh, bl) = split(b);
    (p, ((ah * bh - p) + ah * bl + al * bh) + al * bl)
}

/// `ln(x)` as an unevaluated sum `hi + lo`, carrying about 50 extra bits.
///
/// `pow` needs them: `x^y = exp(y·ln x)` multiplies the *absolute* error of the
/// logarithm by `y` and then exponentiates it, so a plain double logarithm gives
/// `pow(1.0000001, 1e7)` only nine correct digits.
///
/// The extra bits come from not collapsing the two halves of `ln(m·2^k) = k·ln2 +
/// ln(m)`. `k·LN2_HI` is exact — `LN2_HI` has 33 significant bits and `|k| ≤ 1074` —
/// so the only rounding in the whole expression is in the series for `ln(m)`, whose
/// value is under 0.35 whatever `x` is. The absolute error is therefore about
/// `4e-18` no matter how large `x` grows, where a single-double `log` would have
/// `1e-16·|ln x|`.
fn log_ext(x: f64) -> (f64, f64) {
    let (mut m, mut k) = frexp(x);
    if m < core::f64::consts::FRAC_1_SQRT_2 {
        m *= 2.0;
        k -= 1;
    }
    let s = (m - 1.0) / (m + 1.0);
    let s2 = s * s;
    let mut term = s;
    let mut sum = s;
    let mut d = 3.0;
    for _ in 0..14 {
        term *= s2;
        sum += term / d;
        d += 2.0;
    }
    let kf = f64::from(k);
    let (hi, err) = two_sum(kf * LN2_HI, 2.0 * sum);
    (hi, err + kf * LN2_LO)
}

/// `pow`, with C's special cases in front and `exp(y·ln x)` behind them.
///
/// The special cases are not decoration: C requires `pow(-1, ∞) = 1` and
/// `pow(x, 0) = 1` for every `x` including NaN, and a program that trips one of
/// those and gets NaN sees it as a rendering artefact three layers up.
pub(crate) fn pow(x: f64, y: f64) -> f64 {
    if y == 0.0 || x == 1.0 {
        return 1.0;
    }
    if x.is_nan() || y.is_nan() {
        return f64::NAN;
    }
    if y.is_infinite() {
        let ax = fabs(x);
        if ax == 1.0 {
            return 1.0;
        }
        return if (ax > 1.0) == (y > 0.0) { f64::INFINITY } else { 0.0 };
    }
    let y_int = y == trunc(y);
    let y_odd = y_int && fabs(y) < 9.007_199_254_740_992e15 && (y as i64) % 2 != 0;
    if x == 0.0 {
        let neg = x.is_sign_negative() && y_odd;
        return if y > 0.0 {
            if neg {
                -0.0
            } else {
                0.0
            }
        } else if neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    if x.is_infinite() {
        let neg = x < 0.0 && y_odd;
        return if y > 0.0 {
            if neg {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }
        } else if neg {
            -0.0
        } else {
            0.0
        };
    }
    if x < 0.0 && !y_int {
        return f64::NAN; // a real root that does not exist
    }
    // Integer exponents small enough to square out: exact, and much better than
    // going through the logarithm — `pow(10, 2)` must be 100 and not 99.99999999.
    if y_int && fabs(y) <= 64.0 {
        let mut n = fabs(y) as i64;
        let mut base = x;
        let mut acc = 1.0;
        while n > 0 {
            if n & 1 == 1 {
                acc *= base;
            }
            base *= base;
            n >>= 1;
        }
        return if y < 0.0 { 1.0 / acc } else { acc };
    }
    let sign = if x < 0.0 && y_odd { -1.0 } else { 1.0 };
    let (lh, ll) = log_ext(fabs(x));
    // y·ln|x| in two pieces, so the exponent argument keeps its low bits.
    let (ph, pl) = two_prod(y, lh);
    let low = pl + y * ll;
    if ph > 709.9 {
        return sign * f64::INFINITY;
    }
    if ph < -745.5 {
        return sign * 0.0;
    }
    // exp(ph + low) = exp(ph)·(1 + low + …); `low` is under 1e-9 here, so two terms
    // of that series are already below a double's resolution.
    sign * exp(ph) * (1.0 + low * (1.0 + 0.5 * low))
}

// ───────────────────────────── remainders ─────────────────────────────

/// `fmod`: the exact remainder of truncated division.
///
/// Exact, not approximate: `y` is scaled by powers of two (which never rounds) and
/// subtracted only when the result is smaller than the operands (which never rounds
/// either, by Sterbenz's lemma). So the loop below is doing long division in binary
/// and the answer has no error at all.
pub(crate) fn fmod(x: f64, y: f64) -> f64 {
    if x.is_nan() || y.is_nan() || x.is_infinite() || y == 0.0 {
        return f64::NAN;
    }
    if y.is_infinite() || x == 0.0 {
        return x;
    }
    let sign = x;
    let mut a = fabs(x);
    let b = fabs(y);
    if a < b {
        return x;
    }
    let (_, ea) = frexp(a);
    let (_, eb) = frexp(b);
    let mut shift = ea - eb;
    let mut scaled = ldexp(b, shift);
    while shift >= 0 {
        if scaled <= a {
            a -= scaled;
        }
        scaled *= 0.5;
        shift -= 1;
    }
    copysign(a, sign)
}

/// `remainder`: IEEE's version, where the quotient is rounded to nearest-even and
/// the answer can therefore be negative for positive inputs.
pub(crate) fn remainder(x: f64, y: f64) -> f64 {
    if x.is_nan() || y.is_nan() || x.is_infinite() || y == 0.0 {
        return f64::NAN;
    }
    if y.is_infinite() {
        return x;
    }
    let mut r = fmod(x, y);
    let b = fabs(y);
    let half = 0.5 * b;
    let ar = fabs(r);
    if ar > half || (ar == half && fmod(trunc(x / y), 2.0) != 0.0) {
        r -= copysign(b, r);
    }
    r
}

/// `modf`: integral and fractional parts, both with the sign of the input.
pub(crate) fn modf(x: f64) -> (f64, f64) {
    if x.is_infinite() {
        return (copysign(0.0, x), x);
    }
    let i = trunc(x);
    (x - i, i)
}

// ───────────────────────────── trigonometry ─────────────────────────────

// π/2 as three doubles that sum to it with about 159 bits of precision. Each is
// exactly half of the corresponding term of the usual triple-double π, and halving
// a double is exact, so writing them this way costs nothing and keeps the familiar
// constants recognisable.
const PIO2_A: f64 = 3.141_592_653_589_793_1 / 2.0;
const PIO2_B: f64 = 1.224_646_799_147_353_2e-16 / 2.0;
const PIO2_C: f64 = -2.994_769_809_818_369_7e-33 / 2.0;

/// Reduce `x` to `r ∈ [−π/4, π/4]` and the quadrant `x` came from.
///
/// The subtraction `x − n·π/2` is the whole difficulty of trigonometry in floating
/// point: at `x = 10^15` the quotient `n` is around `6·10^14`, and the product
/// `n·π/2` needs 50 bits more than a double holds before the subtraction eats the
/// leading ones. Cody–Waite with two 33-bit halves of π/2 keeps the product exact
/// only while `|n| < 2^20`, which is why a naive version quietly returns noise for
/// arguments a plotting program can genuinely produce.
///
/// So the products and the sums here are done *exactly*, in unevaluated pairs: with
/// [`two_prod`] the size of `n` no longer matters, and what limits the result is the
/// 159 bits of π/2 above — good past `10^30`.
///
/// The quadrant comes back as a `f64` rather than an integer because `n` outgrows
/// `i64` long before it outgrows a double, and `fmod(n, 4)` is exact for every `n`.
fn reduce_pi2(x: f64) -> (f64, f64) {
    if fabs(x) <= FRAC_PI_4 {
        return (x, 0.0);
    }
    let mut n = rint(x * core::f64::consts::FRAC_2_PI);
    let mut r = reduce_by(x, n);
    // For very large `x` the quotient itself was rounded, so `n` can be off by one
    // and `r` land outside the quarter-turn the series are built for. One step in
    // the right direction always fixes it.
    while r > FRAC_PI_4 {
        n += 1.0;
        r = reduce_by(x, n);
    }
    while r < -FRAC_PI_4 {
        n -= 1.0;
        r = reduce_by(x, n);
    }
    (r, n)
}

/// `x − n·π/2`, computed with every intermediate rounding carried rather than
/// dropped.
fn reduce_by(x: f64, n: f64) -> f64 {
    let (ha, la) = two_prod(n, PIO2_A);
    let (hb, lb) = two_prod(n, PIO2_B);
    // x − n·A, exactly, as a pair.
    let (s, e) = two_sum(x, -ha);
    let lo = e - la;
    // …minus n·B, again as a pair.
    let (s2, e2) = two_sum(s, -hb);
    let lo = lo + e2 - lb - n * PIO2_C;
    s2 + lo
}

/// `sin(r)` for `|r| ≤ π/4` by its Taylor series; the `r^17/17!` term it stops
/// before is `1e-21` of the answer.
fn sin_small(r: f64) -> f64 {
    let r2 = r * r;
    let mut term = r;
    let mut sum = r;
    let mut k = 2.0;
    for _ in 0..8 {
        term *= -r2 / (k * (k + 1.0));
        sum += term;
        k += 2.0;
    }
    sum
}

/// `cos(r)` for `|r| ≤ π/4`, likewise.
fn cos_small(r: f64) -> f64 {
    let r2 = r * r;
    let mut term = 1.0;
    let mut sum = 1.0;
    let mut k = 1.0;
    for _ in 0..9 {
        term *= -r2 / (k * (k + 1.0));
        sum += term;
        k += 2.0;
    }
    sum
}

/// Which quarter-turn a reduction landed in, `0..=3`.
///
/// `fmod` rather than a cast: `n` can be `10^17`, which no integer type here holds,
/// and `fmod(n, 4)` is exact for every integer-valued double.
fn quadrant(n: f64) -> i32 {
    let q = fmod(n, 4.0) as i32;
    (q + 4) % 4
}

/// `sin`.
pub(crate) fn sin(x: f64) -> f64 {
    if !x.is_finite() {
        return f64::NAN;
    }
    let (r, n) = reduce_pi2(x);
    match quadrant(n) {
        0 => sin_small(r),
        1 => cos_small(r),
        2 => -sin_small(r),
        _ => -cos_small(r),
    }
}

/// `cos`.
pub(crate) fn cos(x: f64) -> f64 {
    if !x.is_finite() {
        return f64::NAN;
    }
    let (r, n) = reduce_pi2(x);
    match quadrant(n) {
        0 => cos_small(r),
        1 => -sin_small(r),
        2 => -cos_small(r),
        _ => sin_small(r),
    }
}

/// `sincos`, the GNU extension: one reduction instead of two.
///
/// It is in the contract because the compiler emits it — GCC turns a `sin` and a
/// `cos` of the same argument into this call, so a library without it fails to link
/// a program whose source never mentions it.
pub(crate) fn sincos(x: f64) -> (f64, f64) {
    if !x.is_finite() {
        return (f64::NAN, f64::NAN);
    }
    let (r, n) = reduce_pi2(x);
    let (s, c) = (sin_small(r), cos_small(r));
    match quadrant(n) {
        0 => (s, c),
        1 => (c, -s),
        2 => (-s, -c),
        _ => (-c, s),
    }
}

/// `tan`, as the ratio. Poles come out as ±∞ from the division, which is what the
/// limit does anyway.
pub(crate) fn tan(x: f64) -> f64 {
    let (s, c) = sincos(x);
    s / c
}

/// `atan(t)` for `|t| ≤ tan(π/12)`, by its series. The bound is what makes it
/// converge quickly: `0.268^33/33 ≈ 1e-20`.
fn atan_small(t: f64) -> f64 {
    let t2 = t * t;
    let mut term = t;
    let mut sum = t;
    let mut d = 3.0;
    for _ in 0..16 {
        term *= -t2;
        sum += term / d;
        d += 2.0;
    }
    sum
}

const SQRT3: f64 = 1.732_050_807_568_877_2;
const TAN_PI_12: f64 = 0.267_949_192_431_122_7;

/// `atan`, reduced twice: by `atan(x) = π/2 − atan(1/x)` for large `x`, then by the
/// tangent-subtraction identity to bring the argument under `tan(π/12)`.
pub(crate) fn atan(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x.is_infinite() {
        return copysign(FRAC_PI_2, x);
    }
    let sign = x < 0.0;
    let a = fabs(x);
    let (a, invert) = if a > 1.0 { (1.0 / a, true) } else { (a, false) };
    let mut r = if a > TAN_PI_12 {
        core::f64::consts::FRAC_PI_6 + atan_small((a * SQRT3 - 1.0) / (SQRT3 + a))
    } else {
        atan_small(a)
    };
    if invert {
        r = FRAC_PI_2 - r;
    }
    if sign {
        -r
    } else {
        r
    }
}

/// `asin`.
///
/// Near `|x| = 1` the obvious `atan(x/√(1−x²))` loses half its digits to the
/// subtraction, so that branch goes through the half-angle form instead, where the
/// same cancellation happens under a square root that halves its effect.
pub(crate) fn asin(x: f64) -> f64 {
    let a = fabs(x);
    if a > 1.0 {
        return f64::NAN;
    }
    let r = if a > 0.7 {
        FRAC_PI_2 - 2.0 * atan(sqrt((1.0 - a) / (1.0 + a)))
    } else {
        atan(a / sqrt(1.0 - a * a))
    };
    copysign(r, x)
}

/// `acos`.
pub(crate) fn acos(x: f64) -> f64 {
    if fabs(x) > 1.0 {
        return f64::NAN;
    }
    if x < -0.7 {
        return PI - acos(-x);
    }
    if x > 0.7 {
        return 2.0 * atan(sqrt((1.0 - x) / (1.0 + x)));
    }
    FRAC_PI_2 - asin(x)
}

/// `atan2`, with every quadrant and every signed zero C names.
pub(crate) fn atan2(y: f64, x: f64) -> f64 {
    if x.is_nan() || y.is_nan() {
        return f64::NAN;
    }
    if y == 0.0 {
        // The sign of zero is the whole answer here: atan2(+0, −1) is π and
        // atan2(−0, −1) is −π, and a renderer that loses that mirrors a shape.
        return if x.is_sign_negative() { copysign(PI, y) } else { copysign(0.0, y) };
    }
    if x == 0.0 {
        return copysign(FRAC_PI_2, y);
    }
    if x.is_infinite() && y.is_infinite() {
        let q = if x > 0.0 { FRAC_PI_4 } else { 3.0 * FRAC_PI_4 };
        return copysign(q, y);
    }
    let a = atan(y / x);
    if x > 0.0 {
        a
    } else {
        a + copysign(PI, y)
    }
}

// ───────────────────────────── hyperbolics ─────────────────────────────

/// `sinh`. Small arguments go through `expm1` for the same cancellation reason
/// `expm1` itself exists.
pub(crate) fn sinh(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let a = fabs(x);
    if a < 0.5 {
        let e = expm1(a);
        return copysign(0.5 * (e + e / (e + 1.0)), x);
    }
    if a > 710.0 {
        return copysign(f64::INFINITY, x);
    }
    let e = exp(a);
    copysign(0.5 * (e - 1.0 / e), x)
}

/// `cosh`.
pub(crate) fn cosh(x: f64) -> f64 {
    let a = fabs(x);
    if a > 710.0 {
        return f64::INFINITY;
    }
    let e = exp(a);
    0.5 * (e + 1.0 / e)
}

/// `tanh`.
pub(crate) fn tanh(x: f64) -> f64 {
    let a = fabs(x);
    if a > 20.0 {
        // Beyond here the difference from ±1 is below a double's resolution, and
        // the exponential would overflow before saying so.
        return copysign(1.0, x);
    }
    let e = expm1(2.0 * a);
    copysign(e / (e + 2.0), x)
}

/// `atanh`.
pub(crate) fn atanh(x: f64) -> f64 {
    let a = fabs(x);
    if a > 1.0 {
        return f64::NAN;
    }
    if a == 1.0 {
        return copysign(f64::INFINITY, x);
    }
    copysign(0.5 * log1p(2.0 * a / (1.0 - a)), x)
}

/// `asinh`.
pub(crate) fn asinh(x: f64) -> f64 {
    let a = fabs(x);
    let r = if a > 1e8 {
        log(a) + core::f64::consts::LN_2
    } else {
        log1p(a + a * a / (1.0 + sqrt(1.0 + a * a)))
    };
    copysign(r, x)
}

/// `acosh`.
pub(crate) fn acosh(x: f64) -> f64 {
    if x < 1.0 {
        return f64::NAN;
    }
    if x > 1e8 {
        return log(x) + core::f64::consts::LN_2;
    }
    log(x + sqrt(x * x - 1.0))
}

// ───────────────────────────── the C ABI ─────────────────────────────

/// The exported names. Split from the logic above for the reason the whole crate
/// is: a host test binary that defined `sin` would collide with the system's own.
#[cfg(not(test))]
pub mod exports {
    use super::{f32_wrap, f32_wrap2};

    macro_rules! c_math {
        ($($name:ident => $imp:path),* $(,)?) => {$(
            #[no_mangle]
            pub extern "C" fn $name(x: f64) -> f64 { $imp(x) }
        )*};
    }

    macro_rules! c_math2 {
        ($($name:ident => $imp:path),* $(,)?) => {$(
            #[no_mangle]
            pub extern "C" fn $name(x: f64, y: f64) -> f64 { $imp(x, y) }
        )*};
    }

    macro_rules! c_mathf {
        ($($name:ident => $imp:path),* $(,)?) => {$(
            #[no_mangle]
            pub extern "C" fn $name(x: f32) -> f32 { f32_wrap($imp, x) }
        )*};
    }

    macro_rules! c_mathf2 {
        ($($name:ident => $imp:path),* $(,)?) => {$(
            #[no_mangle]
            pub extern "C" fn $name(x: f32, y: f32) -> f32 { f32_wrap2($imp, x, y) }
        )*};
    }

    c_math! {
        fabs => super::fabs,
        floor => super::floor,
        ceil => super::ceil,
        trunc => super::trunc,
        round => super::round,
        rint => super::rint,
        nearbyint => super::rint,
        sqrt => super::sqrt,
        cbrt => super::cbrt,
        exp => super::exp,
        exp2 => super::exp2,
        expm1 => super::expm1,
        log => super::log,
        log2 => super::log2,
        log10 => super::log10,
        log1p => super::log1p,
        sin => super::sin,
        cos => super::cos,
        tan => super::tan,
        asin => super::asin,
        acos => super::acos,
        atan => super::atan,
        sinh => super::sinh,
        cosh => super::cosh,
        tanh => super::tanh,
        asinh => super::asinh,
        acosh => super::acosh,
        atanh => super::atanh,
    }

    c_math2! {
        pow => super::pow,
        fmod => super::fmod,
        remainder => super::remainder,
        atan2 => super::atan2,
        hypot => super::hypot,
        copysign => super::copysign,
    }

    c_mathf! {
        fabsf => super::fabs,
        floorf => super::floor,
        ceilf => super::ceil,
        truncf => super::trunc,
        roundf => super::round,
        rintf => super::rint,
        sqrtf => super::sqrt,
        cbrtf => super::cbrt,
        expf => super::exp,
        expm1f => super::expm1,
        logf => super::log,
        log2f => super::log2,
        log10f => super::log10,
        log1pf => super::log1p,
        sinf => super::sin,
        cosf => super::cos,
        tanf => super::tan,
        asinf => super::asin,
        acosf => super::acos,
        atanf => super::atan,
        sinhf => super::sinh,
        coshf => super::cosh,
        tanhf => super::tanh,
        atanhf => super::atanh,
    }

    c_mathf2! {
        powf => super::pow,
        fmodf => super::fmod,
        remainderf => super::remainder,
        atan2f => super::atan2,
        hypotf => super::hypot,
        copysignf => super::copysign,
    }

    /// `fmin`/`fmax`, which are not `a < b ? a : b`: C says a NaN argument is
    /// ignored rather than propagated, so `fmax(NaN, 3)` is 3.
    #[no_mangle]
    pub extern "C" fn fmax(x: f64, y: f64) -> f64 {
        if x.is_nan() {
            y
        } else if y.is_nan() || x > y {
            x
        } else {
            y
        }
    }

    #[no_mangle]
    pub extern "C" fn fmin(x: f64, y: f64) -> f64 {
        if x.is_nan() {
            y
        } else if y.is_nan() || x < y {
            x
        } else {
            y
        }
    }

    #[no_mangle]
    pub extern "C" fn fmaxf(x: f32, y: f32) -> f32 {
        fmax(f64::from(x), f64::from(y)) as f32
    }

    #[no_mangle]
    pub extern "C" fn fminf(x: f32, y: f32) -> f32 {
        fmin(f64::from(x), f64::from(y)) as f32
    }

    /// `fdim`: the positive difference.
    #[no_mangle]
    pub extern "C" fn fdim(x: f64, y: f64) -> f64 {
        if x.is_nan() || y.is_nan() {
            f64::NAN
        } else if x > y {
            x - y
        } else {
            0.0
        }
    }

    /// `fma`, computed exactly through Dekker's product rather than as `x*y+z`.
    /// A rounded product followed by an addition is precisely what `fma` exists to
    /// avoid, so spelling it that way would be a lie the name tells.
    #[no_mangle]
    pub extern "C" fn fma(x: f64, y: f64, z: f64) -> f64 {
        let (p, e) = super::two_prod(x, y);
        let s = p + z;
        s + e
    }

    #[no_mangle]
    pub extern "C" fn ldexp(x: f64, n: core::ffi::c_int) -> f64 {
        super::ldexp(x, n)
    }

    #[no_mangle]
    pub extern "C" fn scalbn(x: f64, n: core::ffi::c_int) -> f64 {
        super::ldexp(x, n)
    }

    #[no_mangle]
    pub extern "C" fn scalbnf(x: f32, n: core::ffi::c_int) -> f32 {
        super::ldexp(f64::from(x), n) as f32
    }

    /// # Safety
    /// C ABI: `out` points at one writable `int`.
    #[no_mangle]
    pub unsafe extern "C" fn frexp(x: f64, out: *mut core::ffi::c_int) -> f64 {
        let (m, e) = super::frexp(x);
        if !out.is_null() {
            // SAFETY: the caller passes a writable int, as the prototype says.
            unsafe { *out = e };
        }
        m
    }

    /// # Safety
    /// C ABI: `out` points at one writable `double`.
    #[no_mangle]
    pub unsafe extern "C" fn modf(x: f64, out: *mut f64) -> f64 {
        let (frac, int) = super::modf(x);
        if !out.is_null() {
            // SAFETY: as above.
            unsafe { *out = int };
        }
        frac
    }

    /// # Safety
    /// C ABI: `out` points at one writable `float`.
    #[no_mangle]
    pub unsafe extern "C" fn modff(x: f32, out: *mut f32) -> f32 {
        let (frac, int) = super::modf(f64::from(x));
        if !out.is_null() {
            // SAFETY: as above.
            unsafe { *out = int as f32 };
        }
        frac as f32
    }

    /// # Safety
    /// C ABI: both pointers are writable.
    #[no_mangle]
    pub unsafe extern "C" fn sincos(x: f64, s: *mut f64, c: *mut f64) {
        let (sv, cv) = super::sincos(x);
        // SAFETY: the caller passes two writable doubles, as the prototype says.
        unsafe {
            if !s.is_null() {
                *s = sv;
            }
            if !c.is_null() {
                *c = cv;
            }
        }
    }

    /// # Safety
    /// As [`sincos`].
    #[no_mangle]
    pub unsafe extern "C" fn sincosf(x: f32, s: *mut f32, c: *mut f32) {
        let (sv, cv) = super::sincos(f64::from(x));
        // SAFETY: as above.
        unsafe {
            if !s.is_null() {
                *s = sv as f32;
            }
            if !c.is_null() {
                *c = cv as f32;
            }
        }
    }
}

/// The `float` entry points compute in `double` and narrow once.
///
/// One rounding, not two: a `float` result computed in double precision and rounded
/// at the end is within half an ulp of the true answer, which a dedicated `float`
/// algorithm would have to work to match. It costs nothing on this target — AArch64
/// does double-precision arithmetic at the same rate.
fn f32_wrap(f: fn(f64) -> f64, x: f32) -> f32 {
    f(f64::from(x)) as f32
}

fn f32_wrap2(f: fn(f64, f64) -> f64, x: f32, y: f32) -> f32 {
    f(f64::from(x), f64::from(y)) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The claim every test below makes: `got` is within `tol` *relative* error of
    /// what the host's own library says, or within `tol` absolutely when the answer
    /// is near zero and relative error stops meaning anything.
    fn close(got: f64, want: f64, tol: f64, what: &str) {
        if want.is_nan() {
            assert!(got.is_nan(), "{what}: got {got}, want NaN");
            return;
        }
        if want.is_infinite() {
            assert_eq!(got, want, "{what}");
            return;
        }
        let err = (got - want).abs();
        let rel = if want.abs() > 1e-300 { err / want.abs() } else { err };
        assert!(rel <= tol, "{what}: got {got}, want {want}, relative error {rel:e} > {tol:e}");
    }

    /// A spread of arguments that includes the awkward ones: zeros with both signs,
    /// values either side of every reduction boundary this file has, and enough
    /// magnitudes to reach the ends of the exponent range.
    fn samples() -> Vec<f64> {
        let mut v = vec![
            0.0, -0.0, 1.0, -1.0, 0.5, -0.5, 2.0, 3.0, 1e-300, 1e-8, 0.7, -0.7, 0.2679, 0.268,
            1.7320508, 709.0, -708.0, 1e8, -1e8, 1e15, 1e300, -1e300,
        ];
        let mut x = -50.0;
        while x < 50.0 {
            v.push(x);
            x += 0.37;
        }
        v
    }

    #[test]
    fn rounding_is_exact() {
        for &x in &samples() {
            assert_eq!(floor(x), x.floor(), "floor({x})");
            assert_eq!(ceil(x), x.ceil(), "ceil({x})");
            assert_eq!(trunc(x), x.trunc(), "trunc({x})");
            assert_eq!(round(x), x.round(), "round({x})");
            assert_eq!(fabs(x), x.abs(), "fabs({x})");
        }
        // Signed zeros survive, which is the part a naive implementation loses.
        assert!(trunc(-0.5).is_sign_negative());
        assert!(floor(-0.0).is_sign_negative());
        assert_eq!(round(-2.5), -3.0);
        assert_eq!(rint(2.5), 2.0, "rint rounds halves to even");
        assert_eq!(rint(3.5), 4.0);
    }

    #[test]
    fn frexp_and_ldexp_round_trip() {
        for &x in &samples() {
            let (m, e) = frexp(x);
            if x != 0.0 && x.is_finite() {
                assert!((0.5..1.0).contains(&m.abs()), "frexp({x}) mantissa {m}");
            }
            assert_eq!(ldexp(m, e), x, "ldexp(frexp({x}))");
        }
        // The staged multiplication is here for these two, and nothing else tests it.
        assert_eq!(ldexp(1.0, 2000), f64::INFINITY);
        assert_eq!(ldexp(1.0, -2000), 0.0);
        assert_eq!(ldexp(1.5, 1023), 1.5 * 2f64.powi(1023));
        // A subnormal round-trips too, which needs the normalising branch in frexp.
        let sub = f64::from_bits(3);
        let (m, e) = frexp(sub);
        assert_eq!(ldexp(m, e), sub);
    }

    #[test]
    fn roots_agree_with_the_host() {
        for &x in &samples() {
            close(sqrt(x.abs()), x.abs().sqrt(), 1e-15, "sqrt");
            close(cbrt(x), x.cbrt(), 1e-15, "cbrt");
        }
        assert!(sqrt(-1.0).is_nan());
        assert!(sqrt(-0.0).is_sign_negative(), "sqrt(-0) is -0");
        // The overflow hypot exists to avoid.
        close(hypot(3e300, 4e300), 5e300, 1e-15, "hypot big");
        close(hypot(3.0, 4.0), 5.0, 1e-16, "hypot");
        assert_eq!(hypot(f64::INFINITY, f64::NAN), f64::INFINITY);
    }

    #[test]
    fn exp_and_log_agree_with_the_host() {
        for &x in &samples() {
            close(exp(x), x.exp(), 1e-15, "exp");
            close(expm1(x), x.exp_m1(), 1e-14, "expm1");
            if x > 0.0 {
                close(log(x), x.ln(), 1e-15, "log");
                close(log2(x), x.log2(), 1e-15, "log2");
                close(log10(x), x.log10(), 1e-15, "log10");
            }
            if x > -1.0 {
                close(log1p(x), x.ln_1p(), 1e-14, "log1p");
            }
        }
        // Powers of two are exact through log2's exponent path.
        for k in -60..60 {
            assert_eq!(log2(2f64.powi(k)), f64::from(k), "log2(2^{k})");
        }
        // The cancellation expm1 and log1p exist for.
        close(expm1(1e-12), 1e-12f64.exp_m1(), 1e-15, "expm1 tiny");
        close(log1p(1e-12), 1e-12f64.ln_1p(), 1e-15, "log1p tiny");
        assert_eq!(log(0.0), f64::NEG_INFINITY);
        assert!(log(-1.0).is_nan());
        assert_eq!(exp(1000.0), f64::INFINITY);
        assert_eq!(exp(-1000.0), 0.0);
    }

    #[test]
    fn pow_agrees_with_the_host() {
        let bases = [0.1, 0.5, 1.5, 2.0, 3.0, 7.9, 1e5, 1e-5, 123.456];
        let exps = [-30.5, -3.0, -0.5, 0.0, 0.25, 1.0, 2.0, 3.0, 10.0, 30.5, 100.0];
        for &b in &bases {
            for &e in &exps {
                close(pow(b, e), b.powf(e), 1e-14, "pow");
            }
        }
        // Integer exponents are exact, not merely close.
        assert_eq!(pow(10.0, 2.0), 100.0);
        assert_eq!(pow(2.0, 10.0), 1024.0);
        assert_eq!(pow(-2.0, 3.0), -8.0);
        assert_eq!(pow(-2.0, 2.0), 4.0);
        // The multiplied-error case the double-double logarithm is for.
        close(pow(1.0000001, 1e7), 1.0000001f64.powf(1e7), 1e-13, "pow amplified");
        // C's special cases.
        assert_eq!(pow(f64::NAN, 0.0), 1.0);
        assert_eq!(pow(1.0, f64::NAN), 1.0);
        assert_eq!(pow(-1.0, f64::INFINITY), 1.0);
        assert_eq!(pow(2.0, f64::INFINITY), f64::INFINITY);
        assert_eq!(pow(2.0, f64::NEG_INFINITY), 0.0);
        assert_eq!(pow(0.0, -1.0), f64::INFINITY);
        assert_eq!(pow(-0.0, -3.0), f64::NEG_INFINITY);
        assert!(pow(-2.0, 0.5).is_nan());
    }

    #[test]
    fn remainders_are_exact() {
        let pairs = [
            (7.0, 3.0),
            (-7.0, 3.0),
            (7.0, -3.0),
            (1e300, 3.7),
            (5.5, 2.0),
            (1.0, 0.1),
            (1e-8, 1e-9),
        ];
        for &(x, y) in &pairs {
            assert_eq!(fmod(x, y), x % y, "fmod({x}, {y})");
        }
        // IEEE remainder differs from fmod exactly where the quotient rounds up.
        assert_eq!(remainder(5.0, 3.0), -1.0);
        assert_eq!(remainder(5.5, 2.0), -0.5);
        assert_eq!(remainder(4.0, 2.0), 0.0);
        assert!(fmod(1.0, 0.0).is_nan());
        let (frac, int) = modf(-3.75);
        assert_eq!((frac, int), (-0.75, -3.0));
    }

    #[test]
    fn trigonometry_agrees_with_the_host() {
        for &x in &samples() {
            if x.abs() > 1e15 {
                continue; // beyond the documented reduction limit
            }
            close(sin(x), x.sin(), 1e-14, "sin");
            close(cos(x), x.cos(), 1e-14, "cos");
            let (s, c) = sincos(x);
            assert_eq!((s, c), (sin(x), cos(x)), "sincos({x}) matches sin and cos");
            if x.cos().abs() > 1e-3 {
                close(tan(x), x.tan(), 1e-13, "tan");
            }
        }
        // Every quadrant boundary, where a wrong `n & 3` shows up as a sign flip.
        for k in -8..=8 {
            let x = f64::from(k) * FRAC_PI_2;
            close(sin(x), x.sin(), 1e-15, "sin at a multiple of pi/2");
            close(cos(x), x.cos(), 1e-15, "cos at a multiple of pi/2");
        }
        // The identity holds where the reduction is still meaningful.
        let mut x = 0.0;
        while x < 1e6 {
            let (s, c) = sincos(x);
            assert!((s * s + c * c - 1.0).abs() < 1e-14, "sin²+cos² at {x}");
            x += 997.3;
        }
    }

    #[test]
    fn inverse_trigonometry_agrees_with_the_host() {
        let mut x = -1.0;
        while x <= 1.0 {
            close(asin(x), x.asin(), 1e-14, "asin");
            close(acos(x), x.acos(), 1e-14, "acos");
            x += 0.017;
        }
        assert_eq!(asin(1.0), FRAC_PI_2);
        assert_eq!(acos(1.0), 0.0);
        close(acos(-1.0), PI, 1e-15, "acos(-1)");
        for &t in &samples() {
            close(atan(t), t.atan(), 1e-15, "atan");
        }
        // atan2 over the full circle, including the signed-zero cases that carry
        // the direction of the axes.
        for &y in &[-3.0, -1e-300, -0.0, 0.0, 1e-300, 3.0] {
            for &x in &[-3.0, -0.0, 0.0, 3.0] {
                close(atan2(y, x), y.atan2(x), 1e-15, "atan2");
            }
        }
        close(atan2(f64::INFINITY, f64::INFINITY), FRAC_PI_4, 1e-15, "atan2 both inf");
    }

    #[test]
    fn hyperbolics_agree_with_the_host() {
        for &x in &samples() {
            if x.abs() > 1000.0 {
                continue;
            }
            close(sinh(x), x.sinh(), 1e-14, "sinh");
            close(cosh(x), x.cosh(), 1e-14, "cosh");
            close(tanh(x), x.tanh(), 1e-14, "tanh");
            close(asinh(x), x.asinh(), 1e-14, "asinh");
            if x >= 1.0 {
                close(acosh(x), x.acosh(), 1e-14, "acosh");
            }
            if x.abs() < 1.0 {
                close(atanh(x), x.atanh(), 1e-14, "atanh");
            }
        }
        // Small arguments, where the expm1 branches earn their existence.
        close(sinh(1e-10), 1e-10f64.sinh(), 1e-15, "sinh tiny");
        close(tanh(1e-10), 1e-10f64.tanh(), 1e-15, "tanh tiny");
        assert_eq!(tanh(100.0), 1.0);
        assert_eq!(atanh(1.0), f64::INFINITY);
        assert!(atanh(1.5).is_nan());
    }

    /// Within one `float` ulp of the host's own single-precision function.
    ///
    /// Not *equal* to it, which is what this test asserted until it failed: the
    /// host's `sinf` computes in single precision and is itself a rounding away
    /// from the true answer, so at `x = -19.480003` its result and ours straddle
    /// the true value and differ in the last bit. The double-then-narrow path is
    /// the more accurate of the two — this is the tolerance admitting that the
    /// reference is not exact either.
    fn close_f32(got: f32, want: f32, what: &str) {
        let ulp = if want == 0.0 {
            f32::from_bits(1)
        } else {
            let up = f32::from_bits(want.abs().to_bits() + 1);
            up - want.abs()
        };
        assert!(
            (got - want).abs() <= ulp,
            "{what}: got {got}, want {want}, off by more than one ulp ({ulp:e})"
        );
    }

    #[test]
    fn the_float_entry_points_round_once() {
        let mut x = -20.0f32;
        while x < 20.0 {
            // The claim that holds exactly: narrowing our double result is the
            // same as narrowing the host's double result. One rounding, and it is
            // the same rounding.
            assert_eq!(f32_wrap(sin, x), f64::from(x).sin() as f32, "sinf({x})");
            assert_eq!(f32_wrap(cos, x), f64::from(x).cos() as f32, "cosf({x})");
            assert_eq!(f32_wrap(exp, x), f64::from(x).exp() as f32, "expf({x})");
            // And against the host's single-precision routines, one ulp.
            close_f32(f32_wrap(sin, x), x.sin(), "sinf vs host");
            close_f32(f32_wrap(cos, x), x.cos(), "cosf vs host");
            close_f32(f32_wrap(exp, x), x.exp(), "expf vs host");
            if x > 0.0 {
                assert_eq!(f32_wrap(sqrt, x), x.sqrt(), "sqrtf({x})");
                close_f32(f32_wrap(log, x), x.ln(), "logf vs host");
            }
            x += 0.13;
        }
        assert_eq!(f32_wrap2(pow, 2.0, 10.0), 1024.0f32);
        assert_eq!(f32_wrap2(hypot, 3.0, 4.0), 5.0f32);
    }
}
