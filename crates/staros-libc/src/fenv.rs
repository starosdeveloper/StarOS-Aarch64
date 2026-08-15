//! `<fenv.h>` — the floating-point environment, which on AArch64 is two registers.
//!
//! Found by Qt: `qlocale_tools.cpp` line 28 includes `<fenv.h>` on any Linux target,
//! and libstdc++'s own `<fenv.h>` is a wrapper that `#include_next`s the C library's.
//! Nothing in qtbase calls into it from that file — but `<cfenv>` is reachable from
//! anything doing numeric conversion, and this is one of the few parts of a C library
//! that is not a service or a table but a direct view of hardware the program is
//! already running on.
//!
//! ## What is actually here
//!
//! `FPCR` holds the control bits — the rounding mode among them — and `FPSR` holds
//! the sticky exception flags. Both are readable and writable from EL0 without any
//! help from the kernel, so unlike most of this library there is no service behind
//! these functions and no reason for any of them to refuse. They are the real thing.
//!
//! The rounding mode lives in FPCR bits 22–23, and the `FE_*` constants in
//! `<fenv.h>` are *those bit positions*, not 0–3: `FE_UPWARD` is `0x400000`. That is
//! glibc's choice on this architecture and copying it is what lets
//! `fesetround(FE_UPWARD)` be a masked write rather than a table lookup.
//!
//! ## The one thing that is not the hardware's answer
//!
//! Trapping. C lets an implementation raise a *trap* on a floating-point exception,
//! and AArch64 has enable bits in FPCR for it — but they are optional in the
//! architecture and read-as-zero on the cores this targets, including QEMU's. So an
//! exception here is always sticky-flag-only, and `feraiseexcept` sets the flag
//! directly instead of performing an operation chosen to provoke it. The observable
//! difference is nil while traps are unavailable, and this note is where it is
//! recorded in case they ever are not.

use core::ffi::c_int;

/// The exception flags, at their FPSR bit positions. glibc's values on this
/// architecture, and the hardware's own layout — the two agree here, which is why
/// `fetestexcept` is a mask and nothing more.
pub const FE_INVALID: c_int = 0x01;
pub const FE_DIVBYZERO: c_int = 0x02;
pub const FE_OVERFLOW: c_int = 0x04;
pub const FE_UNDERFLOW: c_int = 0x08;
pub const FE_INEXACT: c_int = 0x10;
pub const FE_ALL_EXCEPT: c_int = 0x1f;

/// The rounding modes, at their FPCR bit positions. Not 0–3: the constants *are* the
/// field's value in place, so setting the mode is a masked write.
pub const FE_TONEAREST: c_int = 0x000000;
pub const FE_UPWARD: c_int = 0x400000;
pub const FE_DOWNWARD: c_int = 0x800000;
pub const FE_TOWARDZERO: c_int = 0xc00000;

/// The mask covering the rounding field, so a write to it leaves the rest of FPCR —
/// flush-to-zero, the NaN mode, the trap-enable bits — exactly as it was.
const ROUND_MASK: u64 = 0xc0_0000;

/// `fenv_t`: both registers, in glibc's order and widths.
///
/// 32 bits each even though the system registers are 64: the architecture defines
/// the upper half as reserved, and the ABI a program compiled elsewhere expects is
/// two `unsigned int`s.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FEnv {
    pub fpcr: u32,
    pub fpsr: u32,
}

/// Read FPCR.
#[cfg(target_arch = "aarch64")]
fn fpcr() -> u64 {
    let value: u64;
    // SAFETY: reads a system register available at EL0; touches no memory.
    unsafe { core::arch::asm!("mrs {}, fpcr", out(reg) value, options(nomem, nostack)) };
    value
}

/// Write FPCR.
#[cfg(target_arch = "aarch64")]
fn set_fpcr(value: u64) {
    // SAFETY: writes a system register available at EL0; touches no memory. The
    // caller preserves every field it does not mean to change.
    unsafe { core::arch::asm!("msr fpcr, {}", in(reg) value, options(nomem, nostack)) };
}

/// Read FPSR.
#[cfg(target_arch = "aarch64")]
fn fpsr() -> u64 {
    let value: u64;
    // SAFETY: as `fpcr`.
    unsafe { core::arch::asm!("mrs {}, fpsr", out(reg) value, options(nomem, nostack)) };
    value
}

/// Write FPSR.
#[cfg(target_arch = "aarch64")]
fn set_fpsr(value: u64) {
    // SAFETY: as `set_fpcr`.
    unsafe { core::arch::asm!("msr fpsr, {}", in(reg) value, options(nomem, nostack)) };
}

/// On anything else there are no such registers.
///
/// The host build of this crate exists so the portable half can be tested, and these
/// four functions are the boundary: everything above them is arithmetic on bit
/// fields and is tested, everything below is one instruction and is not. A host
/// build reads zero and discards writes, which makes the *tests* meaningless rather
/// than wrong — and the tests below say so by testing the field arithmetic directly
/// instead of going through the register.
#[cfg(not(target_arch = "aarch64"))]
fn fpcr() -> u64 {
    0
}
#[cfg(not(target_arch = "aarch64"))]
fn set_fpcr(_value: u64) {}
#[cfg(not(target_arch = "aarch64"))]
fn fpsr() -> u64 {
    0
}
#[cfg(not(target_arch = "aarch64"))]
fn set_fpsr(_value: u64) {}

/// The rounding mode a given FPCR selects.
fn round_of(fpcr: u64) -> c_int {
    (fpcr & ROUND_MASK) as c_int
}

/// `fpcr` with its rounding field replaced by `mode`.
fn with_round(fpcr: u64, mode: c_int) -> u64 {
    (fpcr & !ROUND_MASK) | (mode as u64 & ROUND_MASK)
}

/// Whether `mode` names a rounding mode. C says `fesetround` returns nonzero for
/// anything else, and the check has to be against the four values rather than
/// against the mask — `0x400001` is inside the mask and is not a mode.
fn is_round(mode: c_int) -> bool {
    matches!(mode, FE_TONEAREST | FE_UPWARD | FE_DOWNWARD | FE_TOWARDZERO)
}

pub use exports::*;

/// The C entry points.
pub mod exports {
    use super::*;

    /// `FE_DFL_ENV`, the environment a program starts in: round to nearest, no flags
    /// raised, nothing else set.
    ///
    /// It has to be a real object because `FE_DFL_ENV` is a *pointer* to it and
    /// `fesetenv(FE_DFL_ENV)` is the standard way to reset — a null pointer would
    /// have to be special-cased in `fesetenv`, which is where an implementation
    /// starts disagreeing with the header about what null means.
    #[no_mangle]
    pub static __fe_dfl_env: FEnv = FEnv { fpcr: 0, fpsr: 0 };

    /// The rounding mode in force.
    #[no_mangle]
    pub extern "C" fn fegetround() -> c_int {
        round_of(fpcr())
    }

    /// Set the rounding mode. Zero on success, nonzero if `mode` is not one.
    #[no_mangle]
    pub extern "C" fn fesetround(mode: c_int) -> c_int {
        if !is_round(mode) {
            return 1;
        }
        set_fpcr(with_round(fpcr(), mode));
        0
    }

    /// Clear the named sticky flags, leaving the others alone.
    #[no_mangle]
    pub extern "C" fn feclearexcept(mask: c_int) -> c_int {
        let mask = (mask & FE_ALL_EXCEPT) as u64;
        set_fpsr(fpsr() & !mask);
        0
    }

    /// Which of the named flags are set. Returns the intersection, not a boolean —
    /// a caller asking about three exceptions wants to know which one happened.
    #[no_mangle]
    pub extern "C" fn fetestexcept(mask: c_int) -> c_int {
        (fpsr() as c_int) & mask & FE_ALL_EXCEPT
    }

    /// Raise the named exceptions.
    ///
    /// Sets the sticky bits directly rather than performing arithmetic chosen to
    /// provoke them. See the module note: trapping is unavailable here, so the two
    /// are indistinguishable, and the direct form cannot raise an *extra* exception
    /// by accident the way a provoking operation can — `0.0/0.0` sets INVALID, and
    /// on some cores INEXACT along with it.
    #[no_mangle]
    pub extern "C" fn feraiseexcept(mask: c_int) -> c_int {
        let mask = (mask & FE_ALL_EXCEPT) as u64;
        set_fpsr(fpsr() | mask);
        0
    }

    /// Save the named flags into a caller's `fexcept_t`.
    ///
    /// # Safety
    /// C ABI: `out` is valid for one `unsigned int`.
    #[no_mangle]
    pub unsafe extern "C" fn fegetexceptflag(out: *mut u32, mask: c_int) -> c_int {
        if out.is_null() {
            return 1;
        }
        // SAFETY: the caller's contract.
        unsafe { out.write((fpsr() as c_int & mask & FE_ALL_EXCEPT) as u32) };
        0
    }

    /// Restore the named flags from a caller's `fexcept_t`, leaving the rest as they
    /// are — which is what makes this different from writing FPSR.
    ///
    /// # Safety
    /// C ABI: `saved` is valid for one `unsigned int`.
    #[no_mangle]
    pub unsafe extern "C" fn fesetexceptflag(saved: *const u32, mask: c_int) -> c_int {
        if saved.is_null() {
            return 1;
        }
        let mask = (mask & FE_ALL_EXCEPT) as u64;
        // SAFETY: the caller's contract.
        let value = unsafe { saved.read() } as u64;
        set_fpsr((fpsr() & !mask) | (value & mask));
        0
    }

    /// Save the whole environment.
    ///
    /// # Safety
    /// C ABI: `out` is valid for one `fenv_t`.
    #[no_mangle]
    pub unsafe extern "C" fn fegetenv(out: *mut FEnv) -> c_int {
        if out.is_null() {
            return 1;
        }
        // SAFETY: the caller's contract.
        unsafe { out.write(FEnv { fpcr: fpcr() as u32, fpsr: fpsr() as u32 }) };
        0
    }

    /// Restore the whole environment.
    ///
    /// # Safety
    /// C ABI: `env` is valid for one `fenv_t` previously filled by [`fegetenv`] or
    /// [`feholdexcept`].
    #[no_mangle]
    pub unsafe extern "C" fn fesetenv(env: *const FEnv) -> c_int {
        if env.is_null() {
            return 1;
        }
        // SAFETY: the caller's contract.
        let saved = unsafe { env.read() };
        set_fpcr(saved.fpcr as u64);
        set_fpsr(saved.fpsr as u64);
        0
    }

    /// Save the environment and clear the flags, so a block of arithmetic can run
    /// without its exceptions reaching the caller's.
    ///
    /// # Safety
    /// As [`fegetenv`].
    #[no_mangle]
    pub unsafe extern "C" fn feholdexcept(out: *mut FEnv) -> c_int {
        // SAFETY: forwarded from the caller.
        if unsafe { fegetenv(out) } != 0 {
            return 1;
        }
        feclearexcept(FE_ALL_EXCEPT);
        0
    }

    /// Restore an environment and re-raise whatever happened in the meantime.
    ///
    /// The order is the whole point and it is easy to get backwards: the flags
    /// raised *since* `feholdexcept` have to be read before the restore overwrites
    /// them, and re-raised after.
    ///
    /// # Safety
    /// As [`fesetenv`].
    #[no_mangle]
    pub unsafe extern "C" fn feupdateenv(env: *const FEnv) -> c_int {
        let raised = fetestexcept(FE_ALL_EXCEPT);
        // SAFETY: forwarded from the caller.
        if unsafe { fesetenv(env) } != 0 {
            return 1;
        }
        feraiseexcept(raised);
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rounding constants are bit positions, and `with_round` has to place them
    /// without touching anything else.
    ///
    /// Both halves of this are written against a *literal* `0xc0_0000` rather than
    /// against `ROUND_MASK`, and the starting value is every bit outside the field
    /// rather than a plausible-looking FPCR. Both of those are the result of a
    /// falsification that failed to fail.
    ///
    /// Widening `ROUND_MASK` to `0xff_0000` left the first version of this test
    /// passing, for two independent reasons: its sample value `0x0100_1f00` had no
    /// bits in 16–21, so the extra mask bits had nothing to clear; and it compared
    /// `updated & !ROUND_MASK` against `original & !ROUND_MASK`, which uses the
    /// broken mask on both sides and can never disagree. A check that consults the
    /// thing it is checking is not a check.
    ///
    /// In this form the expected result is stated outright — the untouched bits plus
    /// the mode — and any mask that is too wide, too narrow, or shifted fails.
    #[test]
    fn setting_the_rounding_mode_preserves_the_rest_of_fpcr() {
        let original = 0xff3f_ffffu64; // every bit but the rounding field
        for mode in [FE_TONEAREST, FE_UPWARD, FE_DOWNWARD, FE_TOWARDZERO] {
            let updated = with_round(original, mode);
            assert_eq!(
                updated,
                original | mode as u64,
                "mode {mode:#x} did not land cleanly on top of the other bits"
            );
            assert_eq!(round_of(updated), mode, "mode {mode:#x} does not read back");
        }
    }

    /// A value inside the rounding field's mask that is not one of the four modes.
    /// `fesetround` must reject it — a check written as `mode & !ROUND_MASK == 0`
    /// would accept it and leave the hardware in a state no constant names.
    #[test]
    fn a_value_inside_the_mask_is_still_not_a_mode() {
        assert!(!is_round(0x40_0001));
        assert!(!is_round(0x20_0000));
        assert!(is_round(FE_TOWARDZERO));
    }

    /// The exception flags do not overlap and cover exactly `FE_ALL_EXCEPT`. Written
    /// as a sum rather than an or, so a repeated constant — two names given the same
    /// bit, the mistake a hand-copied table invites — fails here.
    #[test]
    fn the_exception_flags_are_five_distinct_bits() {
        let each = [FE_INVALID, FE_DIVBYZERO, FE_OVERFLOW, FE_UNDERFLOW, FE_INEXACT];
        assert_eq!(each.iter().sum::<c_int>(), FE_ALL_EXCEPT);
        assert_eq!(each.iter().fold(0, |a, b| a | b), FE_ALL_EXCEPT);
    }
}
