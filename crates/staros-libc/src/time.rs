//! Layer 3 of the contract: time, over `ClockNow` and `SleepUntil`.
//!
//! There is one clock and it is monotonic — nanoseconds since the kernel started
//! counting. `CLOCK_REALTIME` is answered from the same source, which is a lie of a
//! specific and documented shape: this system has no battery-backed clock and no
//! network, so the only honest wall-clock reading it could give is "unknown", and a
//! program that asks the time to stamp a log line would then have nothing. The
//! epoch is boot; the differences between readings — which is what almost every
//! caller actually uses — are exact.

// Used by the C-ABI half only, which the host build does not compile.
#[cfg(not(test))]
use core::ffi::c_int;
#[cfg(not(test))]
use crate::sys;

/// Nanoseconds in a second.
const NANOS: u64 = 1_000_000_000;

/// C's `struct timespec`, as this library defines it.
#[repr(C)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

/// Split nanoseconds into a `timespec`.
pub(crate) const fn split(nanos: u64) -> (i64, i64) {
    ((nanos / NANOS) as i64, (nanos % NANOS) as i64)
}

/// Join a `timespec` back into nanoseconds, saturating rather than wrapping: a
/// negative or absurd sleep length must not become a short one.
pub(crate) const fn join(sec: i64, nsec: i64) -> u64 {
    if sec < 0 || nsec < 0 {
        return 0;
    }
    let sec = sec as u64;
    let nsec = nsec as u64;
    match sec.checked_mul(NANOS) {
        Some(n) => n.saturating_add(nsec),
        None => u64::MAX,
    }
}

/// C's `struct tm`: a broken-down calendar time.
///
/// The field order and the offsets are C's, not a convenience: `strftime` and every
/// program that fills one in by hand depends on them. `tm_year` counts from 1900 and
/// `tm_mon` from zero, which are the two mistakes everybody makes once.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Tm {
    pub tm_sec: i32,
    pub tm_min: i32,
    pub tm_hour: i32,
    pub tm_mday: i32,
    pub tm_mon: i32,
    pub tm_year: i32,
    pub tm_wday: i32,
    pub tm_yday: i32,
    pub tm_isdst: i32,
    pub tm_gmtoff: i64,
    pub tm_zone: *const i8,
}

/// Days from the Unix epoch to a civil date, and back.
///
/// This is Howard Hinnant's `days_from_civil`: it moves the year's start to March so
/// that the leap day is the last day of the year and the length of a four-hundred
/// year era is a constant 146097 days. The arithmetic is then exact for any year a
/// 64-bit second count can reach, with no table and no loop over years — which is
/// the point, because the loop version is where "the calendar is wrong before 1970"
/// bugs come from.
pub(crate) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (month + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// The inverse: a day count since the epoch back to `(year, month, day)`.
pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Split a count of seconds since the epoch into a UTC `Tm`.
pub(crate) fn tm_from_epoch(seconds: i64) -> Tm {
    // Euclidean division, so that a negative timestamp — any date before 1970 —
    // yields a time of day in `0..86400` rather than a negative hour.
    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    // 1 January 1970 was a Thursday, which is where the 4 comes from.
    let wday = (days + 4).rem_euclid(7);
    let yday = days - days_from_civil(year, 1, 1);
    Tm {
        tm_sec: (rem % 60) as i32,
        tm_min: ((rem / 60) % 60) as i32,
        tm_hour: (rem / 3600) as i32,
        tm_mday: day as i32,
        tm_mon: (month - 1) as i32,
        tm_year: (year - 1900) as i32,
        tm_wday: wday as i32,
        tm_yday: yday as i32,
        tm_isdst: 0,
        tm_gmtoff: 0,
        tm_zone: core::ptr::null(),
    }
}

/// The inverse of [`tm_from_epoch`], and the whole of `mktime`.
///
/// Out-of-range fields are normalised rather than rejected, because that is what
/// `mktime` is *for*: adding 45 to `tm_mday` and calling this is the standard way to
/// do date arithmetic in C, and a version that refused it would be useless.
pub(crate) fn epoch_from_tm(tm: &Tm) -> i64 {
    let year = i64::from(tm.tm_year) + 1900;
    let month = i64::from(tm.tm_mon);
    // Carry the month into the year before the day arithmetic, so tm_mon = 13 means
    // January of the next year rather than a thirteenth month.
    let year = year + month.div_euclid(12);
    let month = month.rem_euclid(12) + 1;
    let days = days_from_civil(year, month, i64::from(tm.tm_mday));
    days * 86_400 + i64::from(tm.tm_hour) * 3600 + i64::from(tm.tm_min) * 60 + i64::from(tm.tm_sec)
}

/// Abbreviated and full names, in C-locale English — the only locale there is.
pub(crate) const DAY_NAMES: [&str; 7] =
    ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
pub(crate) const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Format one `Tm` into `out`, returning how many bytes were written, or `None` if
/// it did not fit.
///
/// C's `strftime` returns 0 when the result does not fit *and* leaves the buffer
/// unspecified — so a caller cannot tell "too small" from "the format produced
/// nothing", and this returns an `Option` internally to keep the two apart until
/// the C boundary insists on merging them.
pub(crate) fn strftime(out: &mut [u8], format: &[u8], tm: &Tm) -> Option<usize> {
    let mut n = 0;
    let mut i = 0;
    while i < format.len() {
        if format[i] != b'%' {
            if !push(out, &mut n, &format[i..=i]) {
                return None;
            }
            i += 1;
            continue;
        }
        i += 1;
        let Some(&conv) = format.get(i) else { break };
        i += 1;
        let year = i64::from(tm.tm_year) + 1900;
        let (mon, mday) = (i64::from(tm.tm_mon) + 1, i64::from(tm.tm_mday));
        let (hour, min, sec) =
            (i64::from(tm.tm_hour), i64::from(tm.tm_min), i64::from(tm.tm_sec));
        let ok = match conv {
            b'%' => push(out, &mut n, b"%"),
            b'n' => push(out, &mut n, b"\n"),
            b't' => push(out, &mut n, b"\t"),
            b'a' => push(out, &mut n, &DAY_NAMES[wrap(tm.tm_wday, 7)].as_bytes()[..3]),
            b'A' => push(out, &mut n, DAY_NAMES[wrap(tm.tm_wday, 7)].as_bytes()),
            b'b' | b'h' => push(out, &mut n, &MONTH_NAMES[wrap(tm.tm_mon, 12)].as_bytes()[..3]),
            b'B' => push(out, &mut n, MONTH_NAMES[wrap(tm.tm_mon, 12)].as_bytes()),
            b'C' => number(out, &mut n, year / 100, 2, b'0'),
            b'd' => number(out, &mut n, mday, 2, b'0'),
            b'e' => number(out, &mut n, mday, 2, b' '),
            b'H' => number(out, &mut n, hour, 2, b'0'),
            b'I' => number(out, &mut n, hour12(tm.tm_hour), 2, b'0'),
            b'j' => number(out, &mut n, i64::from(tm.tm_yday) + 1, 3, b'0'),
            b'm' => number(out, &mut n, mon, 2, b'0'),
            b'M' => number(out, &mut n, min, 2, b'0'),
            b'p' => push(out, &mut n, if tm.tm_hour < 12 { b"AM" } else { b"PM" }),
            b'S' => number(out, &mut n, sec, 2, b'0'),
            b'u' => {
                let d = if tm.tm_wday == 0 { 7 } else { i64::from(tm.tm_wday) };
                number(out, &mut n, d, 1, b'0')
            }
            b'w' => number(out, &mut n, i64::from(tm.tm_wday), 1, b'0'),
            b'y' => number(out, &mut n, year.rem_euclid(100), 2, b'0'),
            b'Y' => number(out, &mut n, year, 1, b'0'),
            b'Z' => push(out, &mut n, b"UTC"),
            b'z' => push(out, &mut n, b"+0000"),
            b's' => number(out, &mut n, epoch_from_tm(tm), 1, b'0'),
            // The composite conversions, spelled out rather than recursed into, so
            // that a buffer running out mid-way stops here rather than half-writing.
            b'D' => {
                number(out, &mut n, mon, 2, b'0')
                    && push(out, &mut n, b"/")
                    && number(out, &mut n, mday, 2, b'0')
                    && push(out, &mut n, b"/")
                    && number(out, &mut n, year.rem_euclid(100), 2, b'0')
            }
            b'F' => {
                number(out, &mut n, year, 4, b'0')
                    && push(out, &mut n, b"-")
                    && number(out, &mut n, mon, 2, b'0')
                    && push(out, &mut n, b"-")
                    && number(out, &mut n, mday, 2, b'0')
            }
            b'R' => {
                number(out, &mut n, hour, 2, b'0')
                    && push(out, &mut n, b":")
                    && number(out, &mut n, min, 2, b'0')
            }
            b'T' => {
                number(out, &mut n, hour, 2, b'0')
                    && push(out, &mut n, b":")
                    && number(out, &mut n, min, 2, b'0')
                    && push(out, &mut n, b":")
                    && number(out, &mut n, sec, 2, b'0')
            }
            // An unknown conversion is copied through verbatim, which is what glibc
            // does and what lets a format string carrying a stray percent survive.
            other => push(out, &mut n, b"%") && push(out, &mut n, &[other]),
        };
        if !ok {
            return None;
        }
    }
    Some(n)
}

/// Append bytes, or report that they do not fit.
fn push(out: &mut [u8], n: &mut usize, bytes: &[u8]) -> bool {
    if *n + bytes.len() > out.len() {
        return false;
    }
    out[*n..*n + bytes.len()].copy_from_slice(bytes);
    *n += bytes.len();
    true
}

/// Append a number, padded to `width` with `pad`.
fn number(out: &mut [u8], n: &mut usize, value: i64, width: usize, pad: u8) -> bool {
    let mut digits = [0u8; 20];
    let mut d = 0;
    let mut v = value.unsigned_abs();
    loop {
        digits[d] = b'0' + (v % 10) as u8;
        v /= 10;
        d += 1;
        if v == 0 {
            break;
        }
    }
    let mut buf = [0u8; 24];
    let mut k = 0;
    if value < 0 {
        buf[k] = b'-';
        k += 1;
    }
    while d + k < width {
        buf[k] = pad;
        k += 1;
    }
    while d > 0 {
        d -= 1;
        buf[k] = digits[d];
        k += 1;
    }
    push(out, n, &buf[..k])
}

/// An index that cannot leave its table, whatever a caller put in the structure.
fn wrap(value: i32, len: usize) -> usize {
    (value.rem_euclid(len as i32)) as usize
}

/// The 12-hour clock, where midnight is 12 rather than 0.
fn hour12(hour: i32) -> i64 {
    let h = hour % 12;
    i64::from(if h == 0 { 12 } else { h })
}

/// The C entry points.
#[cfg(not(test))]
pub mod exports {
    use super::{c_int, epoch_from_tm, join, split, sys, tm_from_epoch, Timespec, Tm};

    /// # Safety
    /// C ABI: `out` is valid for one `Timespec`.
    #[no_mangle]
    pub unsafe extern "C" fn clock_gettime(_clock: c_int, out: *mut Timespec) -> c_int {
        let Some(now) = sys::clock_now() else {
            return -1;
        };
        let (sec, nsec) = split(now);
        // SAFETY: forwarded from the caller.
        unsafe {
            (*out).tv_sec = sec;
            (*out).tv_nsec = nsec;
        }
        0
    }

    /// # Safety
    /// C ABI: `req` is valid for one `Timespec`; `rem` may be null.
    #[no_mangle]
    pub unsafe extern "C" fn nanosleep(req: *const Timespec, rem: *mut Timespec) -> c_int {
        // SAFETY: forwarded from the caller.
        let want = unsafe { join((*req).tv_sec, (*req).tv_nsec) };
        let Some(now) = sys::clock_now() else {
            return -1;
        };
        // The kernel's sleep takes an *absolute* deadline, and this is why: between
        // reading the clock and asking to sleep, this task can be preempted for a
        // whole tick. A relative sleep would then be short by however long that
        // took, and nobody would ever see it.
        sys::sleep_until(now.saturating_add(want));
        if !rem.is_null() {
            // Nothing interrupts a sleep here — there are no signals — so the
            // remainder is always zero rather than unset.
            // SAFETY: forwarded from the caller.
            unsafe {
                (*rem).tv_sec = 0;
                (*rem).tv_nsec = 0;
            }
        }
        0
    }

    /// # Safety
    /// C ABI: `out` may be null.
    #[no_mangle]
    pub unsafe extern "C" fn time(out: *mut i64) -> i64 {
        let seconds = sys::clock_now().map_or(0, |n| split(n).0);
        if !out.is_null() {
            // SAFETY: forwarded from the caller.
            unsafe { *out = seconds };
        }
        seconds
    }

    /// `gettimeofday`, in the shape callers still use.
    ///
    /// # Safety
    /// C ABI: `tv` is valid for two `i64`s; `tz` is ignored.
    #[no_mangle]
    pub unsafe extern "C" fn gettimeofday(tv: *mut i64, _tz: *mut core::ffi::c_void) -> c_int {
        let Some(now) = sys::clock_now() else {
            return -1;
        };
        let (sec, nsec) = split(now);
        // SAFETY: forwarded from the caller.
        unsafe {
            *tv = sec;
            *tv.add(1) = nsec / 1000;
        }
        0
    }

    /// The name of the only time zone here, in the two-element array C declares.
    ///
    /// UTC in both slots: there is no zone database to read and no environment to
    /// read `TZ` from, so local time *is* UTC on this machine. That is a real
    /// property of the system rather than a placeholder — a program that formats a
    /// timestamp gets a correct UTC one, labelled as UTC.
    static UTC: &[u8] = b"UTC\0";
    #[no_mangle]
    pub static mut tzname: [*const core::ffi::c_char; 2] = [core::ptr::null(), core::ptr::null()];
    #[no_mangle]
    pub static mut __tzname: [*const core::ffi::c_char; 2] =
        [core::ptr::null(), core::ptr::null()];
    #[no_mangle]
    pub static mut timezone: i64 = 0;
    #[no_mangle]
    pub static mut daylight: c_int = 0;

    #[no_mangle]
    pub extern "C" fn tzset() {
        let name = UTC.as_ptr().cast::<core::ffi::c_char>();
        // SAFETY: writing two words of a static that C is entitled to read after
        // this call; nothing else in this library touches them.
        unsafe {
            tzname = [name, name];
            __tzname = [name, name];
        }
    }

    /// # Safety
    /// C ABI: `t` points at one `time_t`, `out` at one `struct tm`.
    #[no_mangle]
    pub unsafe extern "C" fn gmtime_r(t: *const i64, out: *mut Tm) -> *mut Tm {
        if t.is_null() || out.is_null() {
            return core::ptr::null_mut();
        }
        // SAFETY: forwarded from the caller.
        unsafe {
            let mut tm = tm_from_epoch(*t);
            tm.tm_zone = UTC.as_ptr().cast::<i8>();
            *out = tm;
        }
        out
    }

    /// Local time, which is UTC here — see [`tzset`].
    ///
    /// # Safety
    /// As [`gmtime_r`].
    #[no_mangle]
    pub unsafe extern "C" fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm {
        // SAFETY: forwarded from the caller.
        unsafe { gmtime_r(t, out) }
    }

    /// The non-reentrant forms, which return a pointer to storage C says stays
    /// valid until the next call. One buffer, shared by both, exactly as glibc does
    /// — and exactly as unsafe across threads, which is why `_r` exists.
    static mut SHARED_TM: Tm = Tm {
        tm_sec: 0,
        tm_min: 0,
        tm_hour: 0,
        tm_mday: 0,
        tm_mon: 0,
        tm_year: 0,
        tm_wday: 0,
        tm_yday: 0,
        tm_isdst: 0,
        tm_gmtoff: 0,
        tm_zone: core::ptr::null(),
    };

    /// # Safety
    /// C ABI: `t` points at one `time_t`.
    #[no_mangle]
    pub unsafe extern "C" fn gmtime(t: *const i64) -> *mut Tm {
        // SAFETY: forwarded from the caller; the shared buffer is C's own design.
        unsafe { gmtime_r(t, core::ptr::addr_of_mut!(SHARED_TM)) }
    }

    /// # Safety
    /// As [`gmtime`].
    #[no_mangle]
    pub unsafe extern "C" fn localtime(t: *const i64) -> *mut Tm {
        // SAFETY: forwarded from the caller.
        unsafe { gmtime(t) }
    }

    /// # Safety
    /// C ABI: `tm` points at one `struct tm`, which this normalises in place.
    #[no_mangle]
    pub unsafe extern "C" fn mktime(tm: *mut Tm) -> i64 {
        if tm.is_null() {
            return -1;
        }
        // SAFETY: forwarded from the caller.
        unsafe {
            let seconds = epoch_from_tm(&*tm);
            // C requires the fields to come back normalised — this is what makes
            // "day 45 of January" turn into 14 February, and it is the reason
            // mktime is the standard way to do date arithmetic in C.
            let mut normalised = tm_from_epoch(seconds);
            normalised.tm_zone = UTC.as_ptr().cast::<i8>();
            *tm = normalised;
            seconds
        }
    }

    /// # Safety
    /// C ABI: `out` is valid for `max` bytes; `format` and `tm` are the caller's.
    #[no_mangle]
    pub unsafe extern "C" fn strftime(
        out: *mut core::ffi::c_char,
        max: usize,
        format: *const core::ffi::c_char,
        tm: *const Tm,
    ) -> usize {
        if out.is_null() || format.is_null() || tm.is_null() || max == 0 {
            return 0;
        }
        // SAFETY: forwarded from the caller.
        unsafe {
            let buf = core::slice::from_raw_parts_mut(out.cast::<u8>(), max);
            let fmt = crate::string::as_bytes(format);
            // One byte held back for the NUL, which C requires and does not count.
            let room = buf.len() - 1;
            match super::strftime(&mut buf[..room], fmt, &*tm) {
                Some(n) => {
                    buf[n] = 0;
                    n
                }
                // Zero means "did not fit", and C says the buffer's contents are
                // then unspecified — but leaving a stale unterminated string there
                // is how a caller that ignores the return value prints garbage.
                None => {
                    buf[0] = 0;
                    0
                }
            }
        }
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub extern "C" fn difftime(a: i64, b: i64) -> f64 {
        (a - b) as f64
    }

    /// `clock`: processor time used, in `CLOCKS_PER_SEC` units. There is no
    /// per-process accounting in the kernel, so this reports elapsed time since
    /// boot — correct for the one thing programs use it for, measuring an interval
    /// between two calls in a single-threaded program, and wrong for the other,
    /// which is comparing CPU time against wall time.
    #[no_mangle]
    pub extern "C" fn clock() -> i64 {
        sys::clock_now().map_or(-1, |n| (n / 1000) as i64)
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn timespec_get(out: *mut Timespec, base: c_int) -> c_int {
        if base != 1 {
            return 0;
        }
        // SAFETY: forwarded from the caller.
        if unsafe { clock_gettime(0, out) } == 0 {
            1
        } else {
            0
        }
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub extern "C" fn timegm(tm: *mut Tm) -> i64 {
        // SAFETY: forwarded from the caller; local time is UTC here.
        unsafe { mktime(tm) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_join_round_trip() {
        for ns in [0u64, 1, 999_999_999, 1_000_000_000, 1_234_567_890_123] {
            let (s, n) = split(ns);
            assert_eq!(join(s, n), ns);
        }
    }

    /// Dates with known answers, including every one where a hand-rolled calendar
    /// goes wrong: the century leap rules, the day before the epoch, and a date far
    /// enough out that a 32-bit day count would have overflowed.
    #[test]
    fn the_calendar_matches_dates_that_are_known() {
        let cases: &[(i64, i64, i64, i64)] = &[
            (0, 1970, 1, 1),
            (86_399, 1970, 1, 1),
            (86_400, 1970, 1, 2),
            (-1, 1969, 12, 31),
            (-86_400, 1969, 12, 31),
            (951_782_400, 2000, 2, 29),   // 2000 was a leap year: divisible by 400
            (4_107_542_400, 2100, 3, 1),  // 2100 is not: divisible by 100
            (1_078_012_800, 2004, 2, 29), // and 2004 is, the ordinary way
            (1_755_043_200, 2025, 8, 13),
            (253_402_300_799, 9999, 12, 31),
        ];
        for &(secs, year, month, day) in cases {
            let tm = tm_from_epoch(secs);
            assert_eq!(
                (i64::from(tm.tm_year) + 1900, i64::from(tm.tm_mon) + 1, i64::from(tm.tm_mday)),
                (year, month, day),
                "tm_from_epoch({secs})"
            );
            // And back, which is the property that actually matters.
            assert_eq!(epoch_from_tm(&tm), secs - secs.rem_euclid(1), "round trip of {secs}");
        }
    }

    /// The two derived fields nothing else checks, against dates whose weekday is
    /// not in dispute.
    #[test]
    fn weekday_and_day_of_year_are_right() {
        // 1 January 1970 was a Thursday (4), and the epoch's own yday is 0.
        let tm = tm_from_epoch(0);
        assert_eq!((tm.tm_wday, tm.tm_yday), (4, 0));
        // 13 August 2025 was a Wednesday.
        let tm = tm_from_epoch(1_755_043_200);
        assert_eq!(tm.tm_wday, 3);
        assert_eq!(tm.tm_yday, 224);
        // 31 December of a leap year is day 365, not 364.
        let tm = tm_from_epoch(978_220_800); // 2000-12-31
        assert_eq!(tm.tm_yday, 365);
        // Before the epoch the weekday must still be in 0..7 — this is the case a
        // truncating remainder gets wrong, by producing -3 for a Sunday.
        for secs in [-1i64, -86_400, -1_000_000_000] {
            let tm = tm_from_epoch(secs);
            assert!((0..7).contains(&tm.tm_wday), "wday {} at {secs}", tm.tm_wday);
            assert!((0..24).contains(&tm.tm_hour), "hour {} at {secs}", tm.tm_hour);
        }
    }

    /// Every day for a century, in both directions. A calendar that is wrong for
    /// one day in 36,524 is wrong in a way that no list of examples finds.
    #[test]
    fn the_calendar_round_trips_for_a_century() {
        let mut day = days_from_civil(1970, 1, 1);
        let end = days_from_civil(2070, 1, 1);
        let mut previous_wday = 3; // the day before the epoch's Thursday
        while day < end {
            let (y, m, d) = civil_from_days(day);
            assert_eq!(days_from_civil(y, m, d), day, "{y}-{m}-{d}");
            let tm = tm_from_epoch(day * 86_400);
            assert_eq!(epoch_from_tm(&tm), day * 86_400);
            // Weekdays advance by exactly one, every day, with no gaps — the check
            // that catches an off-by-one in a leap year rather than in a formula.
            assert_eq!(tm.tm_wday, (previous_wday + 1) % 7, "weekday sequence at {y}-{m}-{d}");
            previous_wday = tm.tm_wday;
            day += 1;
        }
    }

    /// `mktime`'s real job: normalising fields that are out of range.
    #[test]
    fn out_of_range_fields_normalise() {
        // Day 45 of January 2025 is 14 February.
        let tm = Tm { tm_year: 125, tm_mon: 0, tm_mday: 45, ..Default::default() };
        let secs = epoch_from_tm(&tm);
        let back = tm_from_epoch(secs);
        assert_eq!((back.tm_mon, back.tm_mday), (1, 14));
        // Month 13 is January of the next year, not a thirteenth month.
        let tm = Tm { tm_year: 125, tm_mon: 12, tm_mday: 1, ..Default::default() };
        let back = tm_from_epoch(epoch_from_tm(&tm));
        assert_eq!((back.tm_year, back.tm_mon), (126, 0));
        // And negative fields go backwards rather than producing nonsense.
        let tm = Tm { tm_year: 125, tm_mon: -1, tm_mday: 1, ..Default::default() };
        let back = tm_from_epoch(epoch_from_tm(&tm));
        assert_eq!((back.tm_year, back.tm_mon), (124, 11));
    }

    fn format(spec: &str, secs: i64) -> String {
        let tm = tm_from_epoch(secs);
        let mut buf = [0u8; 128];
        let n = strftime(&mut buf, spec.as_bytes(), &tm).expect("fits");
        String::from_utf8(buf[..n].to_vec()).unwrap()
    }

    #[test]
    fn strftime_formats_what_date_would() {
        // The reference on the right is what `date -u -d @1755043200` prints for
        // the same conversion.
        let t = 1_755_043_200; // 2025-08-13 00:00:00 UTC, a Wednesday
        assert_eq!(format("%Y-%m-%d", t), "2025-08-13");
        assert_eq!(format("%F %T", t), "2025-08-13 00:00:00");
        assert_eq!(format("%a %b %e", t), "Wed Aug 13");
        assert_eq!(format("%A, %B %d, %Y", t), "Wednesday, August 13, 2025");
        assert_eq!(format("%j", t), "225");
        assert_eq!(format("%I:%M %p", t), "12:00 AM", "midnight is 12 AM, not 00");
        assert_eq!(format("%I %p", t + 13 * 3600), "01 PM");
        assert_eq!(format("%D", t), "08/13/25");
        assert_eq!(format("%C%y", t), "2025");
        assert_eq!(format("%u %w", t), "3 3");
        assert_eq!(format("%Z %z", t), "UTC +0000");
        assert_eq!(format("%s", t), "1755043200");
        assert_eq!(format("100%% done%n", t), "100% done\n");
        // Sunday is 7 for %u and 0 for %w — the one difference between them.
        let sunday = 1_755_388_800;
        assert_eq!(format("%a %u %w", sunday), "Sun 7 0");
        // An unknown conversion survives instead of eating its argument.
        assert_eq!(format("%Q", t), "%Q");
    }

    #[test]
    fn strftime_refuses_to_overflow_its_buffer() {
        let tm = tm_from_epoch(0);
        let mut buf = [0xAAu8; 8];
        // Exactly the room needed: 8 bytes for "01/01/70".
        assert_eq!(strftime(&mut buf, b"%D", &tm), Some(8));
        // One byte short, and it must refuse rather than truncate — the caller
        // cannot tell a truncated date from a real one.
        let mut small = [0xAAu8; 7];
        assert_eq!(strftime(&mut small, b"%D", &tm), None);
        // Nothing was written past what it managed, and the check is that the tail
        // of the buffer is untouched.
        assert_eq!(small[6], 0xAA);
    }

    #[test]
    fn join_rejects_nonsense_rather_than_wrapping() {
        // A negative sleep must be zero, not a very long one — the wrap is the bug
        // that turns a mistake into a hang.
        assert_eq!(join(-1, 0), 0);
        assert_eq!(join(0, -5), 0);
        assert_eq!(join(i64::MAX, 999_999_999), u64::MAX);
    }
}
