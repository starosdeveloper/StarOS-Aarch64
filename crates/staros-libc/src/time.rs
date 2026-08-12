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

/// The C entry points.
#[cfg(not(test))]
pub mod exports {
    use super::{c_int, join, split, sys, Timespec};

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

    #[test]
    fn join_rejects_nonsense_rather_than_wrapping() {
        // A negative sleep must be zero, not a very long one — the wrap is the bug
        // that turns a mistake into a hang.
        assert_eq!(join(-1, 0), 0);
        assert_eq!(join(0, -5), 0);
        assert_eq!(join(i64::MAX, 999_999_999), u64::MAX);
    }
}
