//! Context switching between kernel tasks.
//!
//! A task switch happens at an ordinary function-call boundary (inside
//! [`context_switch`]), so only the AArch64 callee-saved registers — `x19`..=`x28`,
//! the frame pointer `x29`, the link register `x30`, and `sp` — need to be
//! saved and restored. Caller-saved registers are already dead across the call.
//!
//! A brand-new task is bootstrapped by pointing its saved `x30` at
//! [`__task_trampoline`] and stashing its entry function in `x19`; the first time
//! it is switched to, the trampoline first calls `staros_post_switch` (to settle
//! the task it switched away from, while interrupts are still masked), then enables
//! interrupts and calls the entry.

use core::arch::global_asm;

// The task trampoline below `bl`s `staros_task_exit` when a task's entry
// function returns; that symbol is provided by the kernel scheduler and resolved
// at link time (no Rust-side declaration is needed — the reference is in asm).

extern "C" {
    /// Save the current callee-saved context into `prev` and load `next`.
    fn __context_switch(prev: *mut CpuContext, next: *const CpuContext);
    /// Entry trampoline for freshly-created tasks (see module docs).
    fn __task_trampoline();
}

/// Saved callee-saved CPU state for a suspended task.
///
/// Layout is `#[repr(C)]` and mirrored exactly by the offsets in
/// `__context_switch`: `regs[0..12]` are `x19`..=`x30`, then `sp`, then
/// `TPIDR_EL0`.
#[repr(C)]
#[derive(Debug)]
pub struct CpuContext {
    /// Callee-saved registers `x19`..=`x30` (12 registers).
    regs: [u64; 12],
    /// Stack pointer.
    sp: u64,
    /// `TPIDR_EL0` — the thread pointer EL0 reads for thread-local storage.
    ///
    /// It rides in the context rather than in the scheduler's `Task` because it
    /// *is* thread state, in exactly the way `sp` is: two threads of one process
    /// share every page and every capability, and this register is one of the few
    /// things that must differ between them. Restoring it anywhere other than the
    /// switch would leave a window where a thread runs with its neighbour's
    /// thread pointer — and the symptom of that is `thread_local` variables
    /// aliasing, which looks like memory corruption rather than a scheduling bug.
    ///
    /// Zero until user space sets it, which is what a program with no TLS wants.
    tpidr_el0: u64,
}

impl CpuContext {
    /// An all-zero context, suitable as the destination of the first switch
    /// (its contents are overwritten before it is ever restored).
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            regs: [0; 12],
            sp: 0,
            tpidr_el0: 0,
        }
    }

    /// Prepare a fresh task: on first switch it starts at the trampoline, which
    /// calls `entry`, running on the stack whose top is `stack_top`.
    ///
    /// `stack_top` must be the (16-byte aligned) highest address of the task's
    /// stack region, since the stack grows downward.
    pub fn init(&mut self, entry: extern "C" fn(), stack_top: u64) {
        self.regs = [0; 12];
        self.regs[0] = entry as *const () as u64; // x19 = entry function
        self.regs[11] = __task_trampoline as *const () as u64; // x30 = trampoline
        self.sp = stack_top;
        self.tpidr_el0 = 0;
    }

    /// Set the thread pointer this context restores on its next switch-in.
    pub fn set_tls(&mut self, tls: u64) {
        self.tpidr_el0 = tls;
    }
}

/// Switch the current CPU context to `next`, saving the outgoing state into
/// `prev`. When `prev` is later switched back to, execution resumes right here.
///
/// # Safety
/// Both pointers must reference valid, non-overlapping [`CpuContext`]s. Interrupts
/// must be masked across the call — the switch is not atomic with respect to the
/// scheduler state that selects `prev`/`next`.
pub unsafe fn context_switch(prev: *mut CpuContext, next: *const CpuContext) {
    // SAFETY: forwarded to the caller; `__context_switch` only reads/writes the
    // two contexts and swaps sp/lr.
    unsafe { __context_switch(prev, next) }
}

global_asm!(
    r#"
.section .text
.global __context_switch
// x0 = prev (*mut CpuContext), x1 = next (*const CpuContext)
__context_switch:
    stp     x19, x20, [x0, #0]
    stp     x21, x22, [x0, #16]
    stp     x23, x24, [x0, #32]
    stp     x25, x26, [x0, #48]
    stp     x27, x28, [x0, #64]
    stp     x29, x30, [x0, #80]
    mov     x2, sp
    str     x2, [x0, #96]
    mrs     x2, tpidr_el0    // the outgoing thread's TLS pointer
    str     x2, [x0, #104]

    ldp     x19, x20, [x1, #0]
    ldp     x21, x22, [x1, #16]
    ldp     x23, x24, [x1, #32]
    ldp     x25, x26, [x1, #48]
    ldp     x27, x28, [x1, #64]
    ldp     x29, x30, [x1, #80]
    ldr     x2, [x1, #96]
    mov     sp, x2
    ldr     x2, [x1, #104]
    msr     tpidr_el0, x2    // the incoming thread's TLS pointer
    ret

.global __task_trampoline
__task_trampoline:
    bl      staros_post_switch // settle+reap the task we switched from (IRQs still masked)
    msr     daifclr, #2      // enable IRQs for the newly-started task
    mov     x0, x19          // x19 holds the entry function pointer
    blr     x0
    bl      staros_task_exit // entry returned -> exit the task (never returns)
"#
);
