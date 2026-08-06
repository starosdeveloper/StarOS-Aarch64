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
use core::sync::atomic::{AtomicU32, Ordering};

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
        let count: u64;
        // SAFETY: reading CNTPCT_EL0 is permitted at EL1 and side-effect free.
        unsafe {
            asm!("mrs {c}, cntpct_el0", c = out(reg) count, options(nomem, nostack, preserves_flags));
        }
        count
    }

    fn frequency_hz(&self) -> u64 {
        GenericTimer::frequency_hz()
    }
}
