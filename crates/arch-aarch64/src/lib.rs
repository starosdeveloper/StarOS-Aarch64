//! aarch64 architecture support.
//!
//! This crate owns everything that is specific to the 64-bit Arm architecture
//! and the QEMU `virt` machine we bring up on: the reset vector, early stack
//! setup, and the PL011 UART. It exposes a thin, safe surface to the portable
//! kernel core and keeps the raw `unsafe` assembly and MMIO contained here.
//!
//! Boot contract: [`boot`] defines `_start`, zeroes `.bss`, sets up a stack and
//! calls `rust_start`, which in turn hands control to the kernel's `kmain`
//! (resolved at link time — see [`boot`] for the `extern` declaration).
#![no_std]

pub mod addrspace;
pub mod boot;
pub mod cache;
pub mod context;
pub mod edu;
pub mod exceptions;
pub mod fbinfo;
pub mod gic;
pub mod mailbox;
pub mod mmu;
pub mod pci;
pub mod psci;
pub mod ramfb;
pub mod smmu;
pub mod timer;
pub mod uart;
pub mod usercopy;
pub mod usermode;

/// Park the current core forever in a low-power wait loop.
///
/// Used as the last resort after `kmain` returns or the kernel panics: there is
/// nowhere sensible to go, so we stop drawing power and never come back.
pub fn halt() -> ! {
    loop {
        // SAFETY: `wfe` is always valid at EL1; it merely waits for an event.
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Suspend the core until an interrupt is pending (`wfi`), then return.
///
/// The idle loop uses this to sleep between interrupts instead of spinning.
pub fn wait_for_interrupt() {
    // SAFETY: `wfi` is valid at EL1; it low-power waits for an interrupt and has
    // no effect on program state beyond the wait itself.
    unsafe {
        core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
    }
}
