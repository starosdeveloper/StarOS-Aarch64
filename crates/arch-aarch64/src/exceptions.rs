//! EL1 exception vectors and the trap entry/exit path.
//!
//! AArch64 routes every synchronous fault, IRQ, FIQ and SError through a single
//! 2 KiB-aligned table of 16 entries (`VBAR_EL1`). Each entry is a 128-byte slot
//! — too small for a full context save — so every slot just tags itself with a
//! *kind* and branches to one shared save/restore trampoline (`__exc_common`),
//! which builds a [`TrapFrame`] on the stack and calls [`rust_exception_handler`].
//!
//! This is the seam the scheduler, timer and syscall layer will all plug into:
//! everything that enters the kernel involuntarily arrives here.

use core::arch::{asm, global_asm};
use core::fmt::Write;

// `AcknowledgingController` is the half of the interrupt-controller interface a
// GIC has and an x86 APIC does not: asking the controller *which* interrupt
// fired. See its documentation in `staros_hal` — the split happened when the
// second architecture arrived and could not implement it.
use staros_hal::{AcknowledgingController, InterruptController};

use crate::halt;
use crate::uart::Pl011;

/// The register/state snapshot captured on every exception.
///
/// Layout is `#[repr(C)]` and its size (288 bytes, 16-aligned) is mirrored
/// byte-for-byte by the offsets in `__exc_common` below — keep the two in sync.
#[repr(C)]
#[derive(Debug)]
pub struct TrapFrame {
    /// General-purpose registers `x0`..=`x30`.
    pub regs: [u64; 31],
    /// Exception Syndrome Register (`ESR_EL1`): what happened and why.
    pub esr: u64,
    /// Exception Link Register (`ELR_EL1`): the instruction to return to.
    pub elr: u64,
    /// Saved Program Status Register (`SPSR_EL1`).
    pub spsr: u64,
    /// Fault Address Register (`FAR_EL1`): the faulting address, when relevant.
    pub far: u64,
    /// The user stack pointer (`SP_EL0`) at the moment of the exception.
    ///
    /// This *must* be saved and restored per exception. The hardware does not
    /// bank it away — `SP_EL0` names the same register in EL1 and EL0 — so when a
    /// timer interrupt preempts one EL0 task and the handler switches to another,
    /// the two tasks would otherwise share whatever `SP_EL0` last held. Carrying
    /// it in the frame means each task's `eret` restores *its own* user stack,
    /// which is what makes preemption of a task running in EL0 (rather than parked
    /// in a syscall) sound. Also keeps the frame 16-byte aligned.
    pub sp_el0: u64,
}

/// Exception class value in `ESR_EL1[31:26]` for an `SVC` executed from
/// AArch64 — the trap a userspace (or, for now, kernel) `svc` instruction
/// raises. This is the syscall entry point.
const EC_SVC_AARCH64: u64 = 0x15;

/// Number of register arguments carried into a syscall (`x0`..=`x5`).
pub const SYSCALL_ARG_COUNT: usize = 6;

/// A decoded syscall request handed up to the kernel's dispatcher.
///
/// The arch layer knows the *calling convention* (number in `x8`, arguments in
/// `x0`..=`x5`) but nothing about what each syscall *means* — that policy lives
/// in the kernel crate behind [`staros_syscall_dispatch`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SyscallRequest {
    /// Raw syscall number, taken from `x8`.
    pub number: usize,
    /// Argument registers `x0`..=`x5`.
    pub args: [u64; SYSCALL_ARG_COUNT],
}

extern "Rust" {
    /// The kernel-provided syscall dispatcher. Defined in the `kernel` crate and
    /// resolved at link time, exactly like `kmain` — this keeps syscall *policy*
    /// out of the architecture layer. Returns the value to place in `x0`
    /// (non-negative on success, a negative `KError` code on failure).
    fn staros_syscall_dispatch(req: &SyscallRequest) -> isize;

    /// The kernel-provided IRQ handler, called with the acknowledged interrupt
    /// id after the arch layer has taken it from the GIC. Same link-time seam as
    /// the syscall dispatcher: the arch layer owns the controller, the kernel
    /// owns what each interrupt *means*.
    fn staros_irq_dispatch(intid: u32);

    /// Kernel hook run after the interrupt has been acknowledged *and* EOI'd.
    /// This is where a reschedule may happen: doing it post-EOI means the timer
    /// interrupt is no longer active on the GIC when we switch tasks, so the
    /// task we switch to can receive its own timer interrupts.
    fn staros_irq_epilogue();

    /// The kernel-provided handler for a fault taken from EL0 (a user task
    /// touched memory it may not, executed a bad instruction, etc.). `far`/`esr`
    /// are the fault address and syndrome.
    ///
    /// Returns `true` if the kernel *resolved* the fault — today that means it grew
    /// the task's stack to cover the address — in which case the faulting
    /// instruction is retried. Otherwise the kernel terminates the offending task
    /// and schedules another, and this does not return at all: unlike a kernel
    /// fault, a user fault is never fatal to the system.
    fn staros_user_fault(far: u64, esr: u64) -> bool;
}

/// Unmask IRQs at the PSTATE level (`DAIF.I`). Interrupts only reach the core
/// once both the GIC is enabled *and* this clears the mask.
///
/// # Safety
/// Enables asynchronous interrupt delivery; the vectors and GIC must already be
/// set up so an incoming IRQ is handled rather than lost or fatal.
pub unsafe fn enable_irqs() {
    // SAFETY: `daifclr, #2` clears the I (IRQ) mask bit; permitted at EL1.
    unsafe {
        asm!("msr daifclr, #2", options(nomem, nostack, preserves_flags));
    }
}

/// Mask IRQs at the PSTATE level (`DAIF.I`).
///
/// # Safety
/// Blocks asynchronous interrupt delivery on the current core.
pub unsafe fn disable_irqs() {
    // SAFETY: `daifset, #2` sets the I (IRQ) mask bit; permitted at EL1.
    unsafe {
        asm!("msr daifset, #2", options(nomem, nostack, preserves_flags));
    }
}

/// Wait for an interrupt: put the core into a low-power state until a wakeup
/// event (an interrupt becoming pending — the timer tick or an inter-processor
/// SGI) arrives. This is how an idle core stops burning power spinning.
///
/// A wakeup happens even if `DAIF.I` masks the interrupt, so the caller controls
/// whether the interrupt is then *taken*: enable IRQs around this to have the core
/// actually service the event, mask them to merely poll and re-check.
#[inline]
pub fn wait_for_interrupt() {
    // SAFETY: `wfi` is unprivileged and has no effect beyond suspending the core
    // until a wakeup event; it always resumes.
    unsafe { asm!("wfi", options(nomem, nostack, preserves_flags)) };
}

/// Save the current `DAIF` mask and disable IRQs, returning the previous state
/// for a later [`irq_restore`]. Use to make a critical section IRQ-safe without
/// assuming interrupts were enabled on entry.
///
/// # Safety
/// Alters the interrupt mask; pair every call with exactly one [`irq_restore`].
#[must_use]
pub unsafe fn irq_save() -> u64 {
    let daif: u64;
    // SAFETY: reading DAIF and setting the I bit are permitted at EL1.
    unsafe {
        asm!(
            "mrs {d}, daif",
            "msr daifset, #2",
            d = out(reg) daif,
            options(nomem, nostack, preserves_flags),
        );
    }
    daif
}

/// Restore a `DAIF` mask previously captured by [`irq_save`].
///
/// # Safety
/// `daif` must come from a matching [`irq_save`] on this core.
pub unsafe fn irq_restore(daif: u64) {
    // SAFETY: writing DAIF is permitted at EL1; `daif` is a prior valid value.
    unsafe {
        asm!("msr daif, {d}", d = in(reg) daif, options(nomem, nostack, preserves_flags));
    }
}

/// Install the exception vector table for EL1.
///
/// # Safety
/// Must run at EL1 exactly once during early boot, before any exception can
/// occur. Points `VBAR_EL1` at the table defined in this module.
pub unsafe fn init() {
    extern "C" {
        static __exception_vectors: u8;
    }
    let vbar = &raw const __exception_vectors as u64;
    // SAFETY: writing VBAR_EL1 is permitted at EL1; `vbar` is the address of a
    // 2 KiB-aligned, 16-entry table with the layout the CPU expects. `isb`
    // ensures the new vectors are in effect before we return.
    unsafe {
        asm!(
            "msr vbar_el1, {v}",
            "isb",
            v = in(reg) vbar,
            options(nostack, preserves_flags),
        );
    }
}

/// Turn on **Privileged Access Never** if the CPU implements it (FEAT_PAN, from
/// `ID_AA64MMFR1_EL1.PAN`). Returns whether it was enabled.
///
/// With PAN on, a *privileged* (ordinary EL1) load or store of a page accessible at
/// EL0 faults — so a stray kernel dereference of a user pointer becomes a loud fault
/// instead of a silent info leak. Legitimate user access goes through the
/// unprivileged `LDTR`/`STTR` in [`crate::usercopy`], which PAN does not block.
///
/// Two writes: clear `SCTLR_EL1.SPAN` so PAN is set automatically on every exception
/// entry to EL1 (i.e. throughout syscall handling), and `MSR PAN, #1` so the kernel
/// runs with it set right now. Gated on the feature bit because `MSR PAN` is
/// UNDEFINED on a part without FEAT_PAN (e.g. QEMU's `cortex-a72`), where the kernel
/// simply relies on `LDTR`/`STTR` being correct anyway.
pub fn enable_pan_if_supported() -> bool {
    let mmfr1: u64;
    // SAFETY: reading the ID register is permitted at EL1 and side-effect free.
    unsafe {
        asm!("mrs {v}, id_aa64mmfr1_el1", v = out(reg) mmfr1, options(nomem, nostack, preserves_flags));
    }
    // PAN field is bits [23:20]; 0 means not implemented.
    if (mmfr1 >> 20) & 0xf == 0 {
        return false;
    }
    // SAFETY: FEAT_PAN is present (checked above). Clearing SPAN and setting PAN
    // only tightens access checks on EL0-accessible pages; kernel (EL1-only)
    // mappings and the unprivileged user-copy path are unaffected.
    unsafe {
        asm!(
            "mrs   {t}, sctlr_el1",
            "bic   {t}, {t}, {span}",   // SPAN = bit 23 -> 0: auto-set PAN on trap
            "msr   sctlr_el1, {t}",
            "isb",
            // `MSR PAN, #1` (0xD500419F) by raw encoding: the mnemonic needs the
            // assembler's `+pan` feature, which the bare-metal target does not set,
            // but this runs only where the CPU implements FEAT_PAN.
            ".inst 0xd500419f",         // set it now for kernel steady state
            t = out(reg) _,
            span = const 1u64 << 23,
            options(nostack, preserves_flags),
        );
    }
    true
}

/// Returns the current Exception Level (0..=3) from `CurrentEL`.
#[must_use]
pub fn current_el() -> u64 {
    let el: u64;
    // SAFETY: reading CurrentEL is always permitted and has no side effects.
    unsafe {
        asm!("mrs {e}, CurrentEL", e = out(reg) el, options(nomem, nostack, preserves_flags));
    }
    (el >> 2) & 0b11
}

/// Vector `kind` values are grouped in fours by the source; the low two bits
/// select sync/IRQ/FIQ/SError. `kind % 4 == 0` is synchronous, `== 1` is IRQ.
const KIND_SUBTYPE_MASK: u64 = 0b11;
const SUBTYPE_SYNC: u64 = 0;
const SUBTYPE_IRQ: u64 = 1;

/// Common Rust trap handler, called from `__exc_common` with a pointer to the
/// on-stack [`TrapFrame`] and the vector `kind` (0..=15) that fired.
///
/// Dispatch by exception subtype:
/// - **synchronous** — an `SVC` becomes a syscall (decoded, dispatched, result
///   written back to `x0`); any other synchronous exception is fatal.
/// - **IRQ** — the interrupt is taken from the GIC and handed to the kernel's
///   IRQ dispatcher, then acknowledged (EOI).
///
/// # Safety
/// `frame` must point to a valid, uniquely-owned [`TrapFrame`] built by
/// `__exc_common`. Invoked only from that assembly trampoline.
#[no_mangle]
pub unsafe extern "C" fn rust_exception_handler(frame: *mut TrapFrame, kind: u64) {
    // SAFETY: the trampoline always passes a live, uniquely-owned frame on the
    // current stack; we may mutate it to deliver the syscall return value.
    let frame = unsafe { &mut *frame };

    match kind & KIND_SUBTYPE_MASK {
        SUBTYPE_SYNC => handle_sync(frame, kind),
        SUBTYPE_IRQ => handle_irq(),
        _ => fatal(frame, kind),
    }
}

/// Vector `kind` values 8..=11 are the "Lower EL, AArch64" group — an exception
/// taken from EL0 into the kernel. Used to tell a *user* fault (isolate and kill
/// the task) from a *kernel* fault (fatal to the system).
const KIND_LOWER_EL_FIRST: u64 = 8;
const KIND_LOWER_EL_LAST: u64 = 11;

/// Handle a synchronous exception: `SVC` -> syscall; a fault from EL0 kills that
/// task; any other (kernel) synchronous fault is fatal.
fn handle_sync(frame: &mut TrapFrame, kind: u64) {
    let ec = (frame.esr >> 26) & 0x3f;
    if ec == EC_SVC_AARCH64 {
        let req = SyscallRequest {
            number: frame.regs[8] as usize,
            args: [
                frame.regs[0],
                frame.regs[1],
                frame.regs[2],
                frame.regs[3],
                frame.regs[4],
                frame.regs[5],
            ],
        };
        // SAFETY: `staros_syscall_dispatch` is provided by the linked kernel and
        // takes a shared reference we hold exclusively for the call's duration.
        let ret = unsafe { staros_syscall_dispatch(&req) };
        // `ELR_EL1` already points past the `svc`; write the result into x0 and
        // let the trampoline `eret` back to the caller.
        frame.regs[0] = ret as u64;
        return;
    }
    // A non-SVC synchronous exception taken from EL0 is a faulting user task. The
    // kernel gets first refusal: a touch just below the stack is a *request* for
    // another page, and answering it means returning here so the trampoline `eret`s
    // and the instruction runs again against the now-mapped address. Anything the
    // kernel does not resolve kills that task and switches away, so the call does
    // not return. A fault from the kernel itself stays fatal.
    if (KIND_LOWER_EL_FIRST..=KIND_LOWER_EL_LAST).contains(&kind) {
        // SAFETY: provided by the linked kernel; either resolves the fault or
        // terminates the current task and schedules another, never returning here.
        if unsafe { staros_user_fault(frame.far, frame.esr) } {
            // `ELR_EL1` is untouched, so the `eret` retries the faulting
            // instruction — this is a resumed fault, not a skipped one.
            return;
        }
        unreachable!("staros_user_fault returned false instead of terminating the task");
    }
    fatal(frame, kind);
}

/// Handle an IRQ: acknowledge it at the GIC, dispatch to the kernel, then EOI.
fn handle_irq() {
    if let Some(intid) = crate::gic::controller().acknowledge() {
        // SAFETY: `staros_irq_dispatch` is provided by the linked kernel.
        unsafe { staros_irq_dispatch(intid) };
        crate::gic::controller().end_of_interrupt(intid);
        // Reschedule (if requested) only after EOI — see `staros_irq_epilogue`.
        // SAFETY: provided by the linked kernel; runs in interrupt context with
        // IRQs masked, exactly as a context switch from here requires.
        unsafe { staros_irq_epilogue() };
    }
}

/// Exception class for a data abort taken from a lower EL (EL0 -> EL1).
const EC_DATA_ABORT_LOWER: u64 = 0x24;
/// Exception class for an instruction abort taken from a lower EL.
const EC_INSTR_ABORT_LOWER: u64 = 0x20;

/// Report an unhandled exception and stop the core.
fn fatal(frame: &TrapFrame, kind: u64) -> ! {
    let ec = (frame.esr >> 26) & 0x3f;
    // SAFETY: early-boot single-core context; we own the PL011 (see `kmain`).
    let mut console = unsafe { Pl011::qemu_virt() };
    let _ = writeln!(
        console,
        "\n[exception] kind={kind} ec={ec:#04x} esr={:#018x} elr={:#018x} far={:#018x}",
        frame.esr, frame.elr, frame.far,
    );
    if matches!(ec, EC_DATA_ABORT_LOWER | EC_INSTR_ABORT_LOWER) {
        // A fault from a lower EL means EL0 touched memory it isn't allowed to.
        let _ = writeln!(
            console,
            "[exception] EL0 fault at {:#x}: kernel memory is isolated from user space",
            frame.far,
        );
    }
    let _ = console.write_str("[exception] halting\n");
    halt();
}

// The vector table and the shared save/restore trampoline. The 288-byte frame
// and every offset here must match `TrapFrame` exactly.
global_asm!(
    r#"
// Each vector slot: stash x0, tag the frame with its kind, jump to the common
// trampoline. Four instructions — comfortably inside the 128-byte slot.
.macro VECTOR kind
    .p2align 7
    sub     sp, sp, #288
    str     x0, [sp, #0]
    mov     x0, #\kind
    b       __exc_common
.endm

.section .text
.p2align 11
.global __exception_vectors
__exception_vectors:
    VECTOR 0    // Current EL, SP0,      Synchronous
    VECTOR 1    // Current EL, SP0,      IRQ
    VECTOR 2    // Current EL, SP0,      FIQ
    VECTOR 3    // Current EL, SP0,      SError
    VECTOR 4    // Current EL, SPx,      Synchronous   <- kernel faults / SVC
    VECTOR 5    // Current EL, SPx,      IRQ
    VECTOR 6    // Current EL, SPx,      FIQ
    VECTOR 7    // Current EL, SPx,      SError
    VECTOR 8    // Lower EL,   AArch64,  Synchronous
    VECTOR 9    // Lower EL,   AArch64,  IRQ
    VECTOR 10   // Lower EL,   AArch64,  FIQ
    VECTOR 11   // Lower EL,   AArch64,  SError
    VECTOR 12   // Lower EL,   AArch32,  Synchronous
    VECTOR 13   // Lower EL,   AArch32,  IRQ
    VECTOR 14   // Lower EL,   AArch32,  FIQ
    VECTOR 15   // Lower EL,   AArch32,  SError

// x0 already holds `kind`; the original x0 is saved at [sp, #0].
__exc_common:
    str     x1,  [sp, #8]
    str     x2,  [sp, #16]
    str     x3,  [sp, #24]
    str     x4,  [sp, #32]
    str     x5,  [sp, #40]
    str     x6,  [sp, #48]
    str     x7,  [sp, #56]
    str     x8,  [sp, #64]
    str     x9,  [sp, #72]
    str     x10, [sp, #80]
    str     x11, [sp, #88]
    str     x12, [sp, #96]
    str     x13, [sp, #104]
    str     x14, [sp, #112]
    str     x15, [sp, #120]
    str     x16, [sp, #128]
    str     x17, [sp, #136]
    str     x18, [sp, #144]
    str     x19, [sp, #152]
    str     x20, [sp, #160]
    str     x21, [sp, #168]
    str     x22, [sp, #176]
    str     x23, [sp, #184]
    str     x24, [sp, #192]
    str     x25, [sp, #200]
    str     x26, [sp, #208]
    str     x27, [sp, #216]
    str     x28, [sp, #224]
    str     x29, [sp, #232]
    str     x30, [sp, #240]

    mrs     x1, esr_el1
    str     x1, [sp, #248]
    mrs     x1, elr_el1
    str     x1, [sp, #256]
    mrs     x1, spsr_el1
    str     x1, [sp, #264]
    mrs     x1, far_el1
    str     x1, [sp, #272]
    mrs     x1, sp_el0
    str     x1, [sp, #280]

    mov     x1, x0          // arg1 = kind
    mov     x0, sp          // arg0 = &TrapFrame
    bl      rust_exception_handler

    // Restore return state (the handler may have adjusted elr/spsr). `sp_el0` is
    // restored here too: after a preempting switch this frame belongs to whichever
    // task we are returning into, so its user stack must be reinstated before the
    // `eret`, not left as whatever the preempted task had.
    ldr     x1, [sp, #256]
    msr     elr_el1, x1
    ldr     x1, [sp, #264]
    msr     spsr_el1, x1
    ldr     x1, [sp, #280]
    msr     sp_el0, x1

    ldr     x30, [sp, #240]
    ldr     x29, [sp, #232]
    ldr     x28, [sp, #224]
    ldr     x27, [sp, #216]
    ldr     x26, [sp, #208]
    ldr     x25, [sp, #200]
    ldr     x24, [sp, #192]
    ldr     x23, [sp, #184]
    ldr     x22, [sp, #176]
    ldr     x21, [sp, #168]
    ldr     x20, [sp, #160]
    ldr     x19, [sp, #152]
    ldr     x18, [sp, #144]
    ldr     x17, [sp, #136]
    ldr     x16, [sp, #128]
    ldr     x15, [sp, #120]
    ldr     x14, [sp, #112]
    ldr     x13, [sp, #104]
    ldr     x12, [sp, #96]
    ldr     x11, [sp, #88]
    ldr     x10, [sp, #80]
    ldr     x9,  [sp, #72]
    ldr     x8,  [sp, #64]
    ldr     x7,  [sp, #56]
    ldr     x6,  [sp, #48]
    ldr     x5,  [sp, #40]
    ldr     x4,  [sp, #32]
    ldr     x3,  [sp, #24]
    ldr     x2,  [sp, #16]
    ldr     x1,  [sp, #8]
    ldr     x0,  [sp, #0]
    add     sp, sp, #288
    eret
"#
);
