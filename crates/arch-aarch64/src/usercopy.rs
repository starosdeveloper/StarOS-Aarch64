//! Copying to and from user (EL0) memory the way the architecture intends.
//!
//! The kernel runs at EL1 and a user pointer names a page in the *EL0* translation
//! regime (TTBR0). A plain EL1 load/store of such a page works on parts without
//! **Privileged Access Never** (FEAT_PAN) — which is exactly why the old code could
//! dereference user pointers directly and why it would break on a real Cortex-A76,
//! where PAN is present: with PAN set, a *privileged* access to an EL0-accessible
//! page faults. That is the whole point of PAN (it stops the kernel being tricked
//! into reading kernel-forbidden memory through a user alias), and the sanctioned
//! escape hatch is the **unprivileged** load/store instructions `LDTR`/`STTR`:
//! executed at EL1 they perform the access *as if at EL0*, so they honour the EL0
//! permission bits and are unaffected by PAN.
//!
//! These routines therefore work identically with or without PAN, on QEMU
//! `cortex-a72` and on a Pi 5, which is why the kernel can always use them. The
//! caller must still have validated (via a page-table walk) that the range is
//! mapped and EL0-accessible — `LDTR`/`STTR` fault just like any access when it is
//! not, and that fault would be taken at EL1.

use core::arch::asm;

/// Copy `dst.len()` bytes from the EL0 address `user_src` into `dst`, byte by byte
/// with unprivileged loads.
///
/// # Safety
/// `[user_src, user_src + dst.len())` must be mapped and readable at EL0 in the
/// currently active address space (the caller confirms this with a table walk).
pub unsafe fn copy_from_user(dst: &mut [u8], user_src: u64) {
    for (i, slot) in dst.iter_mut().enumerate() {
        let byte: u64;
        // SAFETY: `LDTR` performs an EL0-permissioned byte load; the caller
        // guarantees the address is mapped EL0-readable.
        unsafe {
            asm!(
                "ldtrb {b:w}, [{a}]",
                b = out(reg) byte,
                a = in(reg) user_src + i as u64,
                options(nostack, readonly, preserves_flags),
            );
        }
        *slot = byte as u8;
    }
}

/// Copy `src` into the EL0 address `user_dst`, byte by byte with unprivileged
/// stores.
///
/// # Safety
/// `[user_dst, user_dst + src.len())` must be mapped and writable at EL0 in the
/// currently active address space (the caller confirms this with a table walk).
pub unsafe fn copy_to_user(user_dst: u64, src: &[u8]) {
    for (i, &byte) in src.iter().enumerate() {
        // SAFETY: `STTR` performs an EL0-permissioned byte store; the caller
        // guarantees the address is mapped EL0-writable.
        unsafe {
            asm!(
                "sttrb {b:w}, [{a}]",
                b = in(reg) u32::from(byte),
                a = in(reg) user_dst + i as u64,
                options(nostack, preserves_flags),
            );
        }
    }
}
