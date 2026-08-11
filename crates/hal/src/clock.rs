//! Turning raw counter ticks into monotonic nanoseconds.
//!
//! A hardware counter counts at whatever rate the machine felt like: 62.5 MHz on
//! QEMU `virt`, 54 MHz on a BCM2712, 19.2 MHz on older Broadcom parts, 24 MHz on
//! plenty of others. Everything above the driver wants nanoseconds. The
//! conversion is `ns = ticks * 1e9 / freq`, and it is exactly the kind of
//! arithmetic that looks trivial and is not:
//!
//! - `ticks * 1_000_000_000` overflows a `u64` after `u64::MAX / 1e9` ticks —
//!   nine minutes at 33 MHz. A clock that wraps that far into a boot is worse
//!   than no clock, because it looks like it works. Reducing the fraction saves
//!   most frequencies (54 MHz becomes 500/27, good for centuries) but not one
//!   coprime with 1e9, which does not reduce at all.
//! - `ticks / freq * 1_000_000_000` does not overflow and is useless: it
//!   quantises time to whole seconds.
//! - Precomputing `1e9 / freq` as an integer throws away everything below one
//!   nanosecond per tick, which at 54 MHz is a 3.7 % error — a "60 fps"
//!   animation running at 58.
//!
//! So the scale is kept as a *reduced fraction* `num/den` and applied in 128-bit
//! arithmetic, which is exact for every input and saturates rather than wraps at
//! the far end. At most frequencies the fraction reduces to something with
//! `den == 1` (62.5 MHz becomes exactly 16 ns per tick), and [`TickScale::is_exact`]
//! says so — worth printing once at boot, because "this machine's counter divides
//! a nanosecond evenly" and "it does not" are different debugging worlds.
//!
//! This module is pure arithmetic on purpose: it names no register and touches no
//! hardware, so every rule above is pinned by host tests. The half that has to
//! read `CNTPCT_EL0` lives in the arch crate and contains no arithmetic at all.

/// Nanoseconds in one second — the numerator of every tick conversion.
pub const NANOS_PER_SEC: u64 = 1_000_000_000;

/// The conversion factor from counter ticks to nanoseconds, as a reduced
/// fraction: `nanoseconds = ticks * num / den`.
///
/// Built once from the counter frequency (see [`TickScale::from_hz`]) and then
/// applied per reading. Reduction by the greatest common divisor is not
/// cosmetic: it is what keeps the intermediate product small enough to be cheap
/// and what makes the common frequencies collapse to a plain multiply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TickScale {
    /// Nanoseconds per `den` ticks.
    num: u64,
    /// Ticks per `num` nanoseconds. Never zero.
    den: u64,
}

impl TickScale {
    /// Build the scale for a counter running at `freq_hz`.
    ///
    /// Returns `None` for a frequency of zero — which is exactly what a machine
    /// whose firmware never programmed `CNTFRQ_EL0` reports, and is the one input
    /// that must not be turned into a division.
    #[must_use]
    pub const fn from_hz(freq_hz: u64) -> Option<Self> {
        if freq_hz == 0 {
            return None;
        }
        let g = gcd(NANOS_PER_SEC, freq_hz);
        Some(Self {
            num: NANOS_PER_SEC / g,
            den: freq_hz / g,
        })
    }

    /// Rebuild a scale from parts previously taken out of one.
    ///
    /// Exists because the arch layer stores the two halves in atomics (a
    /// `TickScale` is not `Copy` into an atomic) and has to put them back
    /// together on every reading. Rejects a zero denominator so a half-written
    /// pair can never divide.
    #[must_use]
    pub const fn from_parts(num: u64, den: u64) -> Option<Self> {
        if den == 0 {
            return None;
        }
        Some(Self { num, den })
    }

    /// Nanoseconds per [`denominator`](Self::denominator) ticks.
    #[must_use]
    pub const fn numerator(&self) -> u64 {
        self.num
    }

    /// Ticks per [`numerator`](Self::numerator) nanoseconds.
    #[must_use]
    pub const fn denominator(&self) -> u64 {
        self.den
    }

    /// Whether one tick is a whole number of nanoseconds.
    ///
    /// True at 62.5 MHz (16 ns) and 125 MHz (8 ns); false at 54 MHz, where a tick
    /// is 500/27 ns. Not a correctness property — the conversion is exact either
    /// way — but the sort of fact worth stating once in a boot log, since an
    /// inexact scale is where rounding questions come from later.
    #[must_use]
    pub const fn is_exact(&self) -> bool {
        self.den == 1
    }

    /// Convert a tick count to nanoseconds.
    ///
    /// Exact for every input: the product is formed in 128 bits, so nothing wraps
    /// on the way. A result past `u64::MAX` nanoseconds (584 years) saturates
    /// rather than wrapping — a monotonic clock that goes backwards breaks every
    /// caller that trusted the name, while one that sticks at the end of time
    /// merely stops being useful.
    #[must_use]
    pub const fn nanos(&self, ticks: u64) -> u64 {
        let ns = (ticks as u128 * self.num as u128) / self.den as u128;
        if ns > u64::MAX as u128 {
            u64::MAX
        } else {
            ns as u64
        }
    }

    /// Convert a duration in nanoseconds to counter ticks, **rounding up**.
    ///
    /// The inverse of [`nanos`](Self::nanos), and the direction a timer deadline
    /// travels: a caller asking to be woken in 5 ms needs a tick count to program.
    ///
    /// Rounding up rather than to nearest is deliberate and is the whole
    /// difference between a timer that is late and one that is *early*. Waking a
    /// task before its deadline means the code that checks "has the deadline
    /// passed?" finds that it has not, and either sleeps again (a wasted round
    /// trip) or, worse, treats the reading as time having gone backwards. Being a
    /// tick late is invisible; being a tick early is a bug that only shows up
    /// under load.
    #[must_use]
    pub const fn ticks(&self, nanos: u64) -> u64 {
        let numerator = nanos as u128 * self.den as u128;
        let ticks = numerator.div_ceil(self.num as u128);
        if ticks > u64::MAX as u128 {
            u64::MAX
        } else {
            ticks as u64
        }
    }
}

/// Greatest common divisor, by Euclid. `const` so a scale can be built in a
/// constant.
const fn gcd(a: u64, b: u64) -> u64 {
    let (mut a, mut b) = (a, b);
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frequency QEMU's `virt` machine reports, and the happy case: it
    /// reduces to a whole number of nanoseconds per tick.
    const QEMU_HZ: u64 = 62_500_000;
    /// The BCM2712 (Raspberry Pi 5) system counter — deliberately *not* a divisor
    /// of a nanosecond, which is the case an integer `ns_per_tick` gets wrong.
    const PI5_HZ: u64 = 54_000_000;

    #[test]
    fn zero_frequency_has_no_scale() {
        // The reading a machine whose firmware never set CNTFRQ_EL0 gives. It must
        // not become a division by zero, and it must not silently become 1 Hz.
        assert_eq!(TickScale::from_hz(0), None);
    }

    #[test]
    fn qemu_frequency_reduces_to_whole_nanoseconds() {
        let s = TickScale::from_hz(QEMU_HZ).unwrap();
        assert_eq!((s.numerator(), s.denominator()), (16, 1));
        assert!(s.is_exact());
        assert_eq!(s.nanos(1), 16);
    }

    #[test]
    fn pi5_frequency_is_a_fraction_and_stays_exact() {
        let s = TickScale::from_hz(PI5_HZ).unwrap();
        // 1e9/54e6 reduces to 500/27, not to the 18 an integer division gives.
        assert_eq!((s.numerator(), s.denominator()), (500, 27));
        assert!(!s.is_exact());
        assert_eq!(s.nanos(27), 500);
        // The error an integer 18 ns/tick would accumulate: 3.7 %, i.e. 37 ms
        // every second. One second of ticks must still be one second.
        assert_eq!(s.nanos(PI5_HZ), NANOS_PER_SEC);
        assert_ne!(s.nanos(PI5_HZ), PI5_HZ * 18);
    }

    #[test]
    fn one_second_of_ticks_is_one_second_at_every_plausible_frequency() {
        for hz in [
            1_000_000, 19_200_000, 24_000_000, 25_000_000, 54_000_000, 62_500_000, 100_000_000,
            125_000_000, 1_000_000_000,
        ] {
            let s = TickScale::from_hz(hz).unwrap();
            assert_eq!(s.nanos(hz), NANOS_PER_SEC, "one second at {hz} Hz");
            assert_eq!(s.nanos(hz * 60), 60 * NANOS_PER_SEC, "one minute at {hz} Hz");
        }
    }

    #[test]
    fn a_frequency_that_does_not_divide_a_nanosecond_still_reduces() {
        // 3 MHz: gcd(1e9, 3e6) = 1e6, so 1000/3 — a fraction, and the reduction
        // matters because an unreduced 1e9/3e6 would overflow far sooner.
        let s = TickScale::from_hz(3_000_000).unwrap();
        assert_eq!((s.numerator(), s.denominator()), (1000, 3));
        assert_eq!(s.nanos(3), 1000);
    }

    #[test]
    fn conversion_never_wraps_and_never_goes_backwards() {
        // The frequency that makes the 128-bit product necessary. Reduction alone
        // saves the common cases — 54 MHz reduces the numerator from 1e9 to 500,
        // buying centuries — but a frequency coprime with 1e9 does not reduce at
        // all, so the numerator stays 1e9 and a 64-bit product wraps after
        // `u64::MAX / 1e9` ticks: 553 seconds, i.e. nine minutes into a boot.
        const COPRIME_HZ: u64 = 33_333_333;
        let s = TickScale::from_hz(COPRIME_HZ).unwrap();
        assert_eq!(s.numerator(), NANOS_PER_SEC, "this frequency must not reduce");

        let mut previous = 0;
        for seconds in [0, 1, 100, 553, 554, 1_000, 86_400, 3_153_600] {
            let ticks = seconds * COPRIME_HZ;
            let ns = s.nanos(ticks);
            assert!(ns >= previous, "went backwards at {seconds}s: {ns} < {previous}");
            // Exact to the nanosecond, checked against the same arithmetic done
            // in 128 bits — the answer a wrapping product cannot produce.
            let expected = (ticks as u128 * NANOS_PER_SEC as u128 / COPRIME_HZ as u128) as u64;
            assert_eq!(ns, expected, "wrong at {seconds}s");
            previous = ns;
        }

        // And the reduced case stays right over the same span, so the test covers
        // both sides of the reduction rather than only the hard one.
        let pi5 = TickScale::from_hz(PI5_HZ).unwrap();
        for seconds in [0u64, 553, 3_153_600] {
            assert_eq!(pi5.nanos(seconds * PI5_HZ), seconds * NANOS_PER_SEC);
        }
    }

    #[test]
    fn the_far_end_saturates_rather_than_wrapping() {
        let s = TickScale::from_hz(QEMU_HZ).unwrap();
        // u64::MAX ticks at 16 ns each is sixteen times what a u64 of nanoseconds
        // can hold. The answer must be the end of time, not a small number.
        assert_eq!(s.nanos(u64::MAX), u64::MAX);
        // And the last value that genuinely fits must not be clamped early.
        let last_exact = u64::MAX / 16;
        assert_eq!(s.nanos(last_exact), last_exact * 16);
    }

    #[test]
    fn nanoseconds_convert_back_to_ticks_never_early() {
        for hz in [19_200_000, 24_000_000, 54_000_000, 62_500_000, 33_333_333] {
            let s = TickScale::from_hz(hz).unwrap();
            // A whole second is a whole second in both directions.
            assert_eq!(s.ticks(NANOS_PER_SEC), hz, "one second at {hz} Hz");
            // And every duration must produce enough ticks to reach it — the
            // property that matters for a deadline. Rounding down would let a
            // sleeper wake before the time it asked for.
            for ns in [1, 2, 17, 999, 1_000, 1_000_000, 16_666_667, 123_456_789] {
                let ticks = s.ticks(ns);
                assert!(
                    s.nanos(ticks) >= ns,
                    "{hz} Hz: {ns} ns -> {ticks} ticks -> {} ns, which is early",
                    s.nanos(ticks),
                );
                // Not wastefully many, either: one tick fewer must fall short.
                if ticks > 0 {
                    assert!(s.nanos(ticks - 1) < ns, "{hz} Hz: {ns} ns rounded up too far");
                }
            }
        }
    }

    #[test]
    fn a_zero_duration_is_zero_ticks() {
        // The "deadline already passed" case. It must not round up to one tick, or
        // a caller polling with a zero timeout would sleep.
        let s = TickScale::from_hz(PI5_HZ).unwrap();
        assert_eq!(s.ticks(0), 0);
    }

    #[test]
    fn from_parts_rejects_a_zero_denominator() {
        // What a half-initialised pair of atomics looks like. It must fail rather
        // than divide by zero.
        assert_eq!(TickScale::from_parts(16, 0), None);
        assert_eq!(
            TickScale::from_parts(16, 1),
            Some(TickScale::from_hz(QEMU_HZ).unwrap())
        );
    }

    #[test]
    fn gcd_is_euclid() {
        assert_eq!(gcd(1_000_000_000, 62_500_000), 62_500_000);
        assert_eq!(gcd(1_000_000_000, 54_000_000), 2_000_000);
        assert_eq!(gcd(17, 5), 1);
        assert_eq!(gcd(0, 7), 7);
        assert_eq!(gcd(7, 0), 7);
    }
}
