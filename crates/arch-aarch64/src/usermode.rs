//! Dropping from EL1 to EL0 (entering user mode).
//!
//! Switching to a lower Exception Level on AArch64 is done with `eret`: we stage
//! the target state in the exception-return registers — `ELR_EL1` (the user PC),
//! `SP_EL0` (the user stack), and `SPSR_EL1` (the target PSTATE) — then `eret`
//! consumes them and lands at EL0. The reverse trip is any exception (e.g. an
//! `svc`), which re-enters the kernel through the vectors installed at EL1.

use core::arch::asm;

/// `SPSR_EL1` value for returning to EL0 using `SP_EL0` (mode `EL0t`) with IRQs
/// **enabled** (Debug/SError/FIQ still masked). Leaving `I` clear is what lets
/// the timer preempt a running user task, so the scheduler can multiplex several
/// EL0 processes — the whole point of running them as scheduler tasks.
const SPSR_EL0T: u64 = (1 << 9) | (1 << 8) | (1 << 6); // D, A, F masked; I clear; EL0t

/// Drop to EL0 and begin executing `entry` on stack `user_sp`, with `arg` in
/// `x0`. Never returns: control only comes back to the kernel via an exception (a
/// syscall, or the timer IRQ that preempts the task).
///
/// `arg` exists for threads. A process starts at its ELF entry with nothing to
/// say, but a thread is created *by* code that has something to hand it — the
/// address of whatever it is meant to work on. Passing it in `x0` is the ordinary
/// AArch64 calling convention, so the thread's entry reads like a function.
///
/// # Safety
/// `entry` must point to valid, EL0-executable code and `user_sp` to a valid,
/// EL0-writable, 16-byte aligned stack top — both mapped into the currently
/// active address space (see [`crate::addrspace`]). The EL1 exception vectors
/// must already be installed to catch the user's traps.
pub unsafe fn enter_el0(entry: u64, user_sp: u64, arg: u64) -> ! {
    // SAFETY: stages the exception-return state and performs `eret` to EL0. The
    // caller guarantees `entry`/`user_sp` are valid EL0 mappings.
    unsafe {
        asm!(
            "msr sp_el0, {sp}",
            "msr elr_el1, {entry}",
            "msr spsr_el1, {spsr}",
            "isb",
            "eret",
            sp = in(reg) user_sp,
            entry = in(reg) entry,
            spsr = in(reg) SPSR_EL0T,
            // The argument goes straight into the register EL0 will see, rather
            // than through a `mov` that would need `x0` as an output — which the
            // `noreturn` option forbids, since nothing comes back to read it.
            in("x0") arg,
            options(noreturn),
        );
    }
}
