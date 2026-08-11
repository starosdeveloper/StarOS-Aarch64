//! ARM Generic Timer (EL1 physical timer, `CNTP`).
//!
//! The generic timer is a per-core down-counter driven at a fixed frequency
//! (`CNTFRQ_EL0`). Writing `CNTP_TVAL_EL0` arms it to fire after that many
//! ticks; when it reaches zero it raises its private peripheral interrupt. On
//! Its interrupt is a PPI — a line private to each core — and which id it is
//! comes from the device tree (see [`set_intid`]), not from a constant.
//!
//! This module is pure mechanism: *when* to reload and *what a tick means* is
//! the kernel's policy.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use staros_hal::clock::TickScale;
use staros_hal::Timer;

/// Fallback interrupt id of the EL1 physical timer: PPI 14, i.e. `16 + 14`.
///
/// The device tree's `arm,armv8-timer` node declares this, and [`set_intid`]
/// records what it said. This value is only what a machine with no usable tree
/// gets — right for QEMU `virt` and for most ARM designs, but a guess.
pub const TIMER_INTID_FALLBACK: u32 = 30;

/// The timer's interrupt id, as the machine reported it.
static TIMER_INTID: AtomicU32 = AtomicU32::new(TIMER_INTID_FALLBACK);

/// Record the interrupt id the device tree gave for the EL1 physical timer.
///
/// # Safety
/// Call once, during early boot on the primary core, before the timer line is
/// enabled or any core arms its timer.
pub unsafe fn set_intid(intid: u32) {
    TIMER_INTID.store(intid, Ordering::Relaxed);
}

/// The interrupt id every core enables and acknowledges for its own timer.
#[must_use]
pub fn intid() -> u32 {
    TIMER_INTID.load(Ordering::Relaxed)
}

/// Numerator of the tick→nanosecond scale, or 0 before [`init_monotonic`].
static SCALE_NUM: AtomicU64 = AtomicU64::new(0);
/// Denominator of the tick→nanosecond scale, or 0 before [`init_monotonic`].
static SCALE_DEN: AtomicU64 = AtomicU64::new(0);
/// Counter reading at [`init_monotonic`], subtracted from every later one so the
/// clock starts near zero instead of at whatever the firmware had already
/// counted.
static BASE_TICKS: AtomicU64 = AtomicU64::new(0);
/// Set once both halves of the scale and the base are stored. Readers check this
/// rather than the values, so a reading can never land between the two stores.
static MONOTONIC_READY: AtomicBool = AtomicBool::new(false);

/// Start the monotonic clock: record the counter frequency as a scale and take
/// the base reading every later one is measured from.
///
/// Returns the scale (for the boot log), or `None` if the machine reports a zero
/// counter frequency — firmware that never programmed `CNTFRQ_EL0`. That case is
/// reported and left off rather than papered over with a guessed frequency: a
/// clock that is confidently wrong is worse than one that says it is absent.
///
/// The system counter is common to all cores, so the base is taken once and is
/// valid everywhere; there is no per-core copy to keep in step.
///
/// # Safety
/// Call once, on the primary core during boot, before any other core reads the
/// monotonic clock.
pub unsafe fn init_monotonic() -> Option<TickScale> {
    let scale = TickScale::from_hz(GenericTimer::frequency_hz())?;
    SCALE_NUM.store(scale.numerator(), Ordering::Relaxed);
    SCALE_DEN.store(scale.denominator(), Ordering::Relaxed);
    BASE_TICKS.store(GenericTimer::counter(), Ordering::Relaxed);
    // Release: the three stores above must be visible to any core that observes
    // this flag, or a secondary could read a zero denominator.
    MONOTONIC_READY.store(true, Ordering::Release);
    Some(scale)
}

/// Nanoseconds since [`init_monotonic`], or `None` if the clock never started.
///
/// Non-decreasing by construction: the counter itself only counts up, and the
/// subtraction saturates, so even a reading from before the base (impossible on
/// one counter, but not worth trusting a register for) yields zero rather than an
/// enormous number.
#[must_use]
pub fn monotonic_ns() -> Option<u64> {
    if !MONOTONIC_READY.load(Ordering::Acquire) {
        return None;
    }
    let scale = TickScale::from_parts(
        SCALE_NUM.load(Ordering::Relaxed),
        SCALE_DEN.load(Ordering::Relaxed),
    )?;
    let base = BASE_TICKS.load(Ordering::Relaxed);
    Some(scale.nanos(GenericTimer::counter().saturating_sub(base)))
}

/// How many counter ticks `nanos` nanoseconds are, rounded **up** so a deadline
/// programmed from it is never early. `None` if the clock never started.
///
/// The counterpart of [`monotonic_ns`], and what turns "wake me at time T" into
/// something [`GenericTimer::arm`] accepts.
#[must_use]
pub fn ticks_from_nanos(nanos: u64) -> Option<u64> {
    Some(monotonic_scale()?.ticks(nanos))
}

/// The tick→nanosecond scale in force, once [`init_monotonic`] has run.
#[must_use]
pub fn monotonic_scale() -> Option<TickScale> {
    if !MONOTONIC_READY.load(Ordering::Acquire) {
        return None;
    }
    TickScale::from_parts(
        SCALE_NUM.load(Ordering::Relaxed),
        SCALE_DEN.load(Ordering::Relaxed),
    )
}

/// Handle to the per-core generic timer.
pub struct GenericTimer;

impl GenericTimer {
    /// Read the counter frequency in Hz from `CNTFRQ_EL0`.
    #[must_use]
    pub fn frequency_hz() -> u64 {
        let freq: u64;
        // SAFETY: reading CNTFRQ_EL0 is permitted at EL1 and has no side effect.
        unsafe {
            asm!("mrs {f}, cntfrq_el0", f = out(reg) freq, options(nomem, nostack, preserves_flags));
        }
        freq
    }

    /// Read the system counter (`CNTPCT_EL0`).
    ///
    /// The `isb` is not decoration. `CNTPCT_EL0` is permitted to be read
    /// speculatively and out of order with respect to the instructions around it,
    /// so without a barrier two readings taken either side of some work can come
    /// back in the wrong order — a clock that appears to go backwards over short
    /// intervals, which is precisely where a monotonic clock is used. The
    /// architecture's answer, and Linux's, is an `isb` before the read.
    #[must_use]
    pub fn counter() -> u64 {
        let count: u64;
        // SAFETY: reading CNTPCT_EL0 is permitted at EL1 and side-effect free;
        // `isb` orders it against preceding instructions.
        unsafe {
            asm!(
                "isb",
                "mrs {c}, cntpct_el0",
                c = out(reg) count,
                options(nomem, nostack, preserves_flags),
            );
        }
        count
    }

    /// Arm the timer to fire once after `ticks` counter ticks and enable it.
    ///
    /// # Safety
    /// Programs the per-core timer control registers; call at EL1 with the
    /// timer interrupt already routed through the GIC.
    pub unsafe fn arm(ticks: u64) {
        // SAFETY: writing CNTP_TVAL_EL0/CNTP_CTL_EL0 is permitted at EL1; TVAL
        // sets the countdown and CTL=1 enables the timer (IMASK clear).
        unsafe {
            asm!(
                "msr cntp_tval_el0, {t}",
                "mov {tmp}, #1",
                "msr cntp_ctl_el0, {tmp}",
                t = in(reg) ticks,
                tmp = out(reg) _,
                options(nostack, preserves_flags),
            );
        }
    }

    /// Disable the timer so it stops raising interrupts.
    ///
    /// # Safety
    /// Writes the per-core timer control register; call at EL1.
    pub unsafe fn disable() {
        // SAFETY: clearing CNTP_CTL_EL0 (ENABLE=0) is permitted at EL1.
        unsafe {
            asm!("msr cntp_ctl_el0, xzr", options(nostack, preserves_flags));
        }
    }
}

impl Timer for GenericTimer {
    fn now_ticks(&self) -> u64 {
        GenericTimer::counter()
    }

    fn frequency_hz(&self) -> u64 {
        GenericTimer::frequency_hz()
    }
}
