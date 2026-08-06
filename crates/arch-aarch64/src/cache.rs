//! Cache maintenance for making freshly-written code executable.
//!
//! When the kernel *loads* a program — copying instruction bytes through the
//! normal (data) memory path and then jumping to them — the write lands in the
//! data cache while the instruction fetch path may still see stale memory or a
//! stale instruction cache. The architecture requires an explicit sequence to
//! make the two coherent to the Point of Unification: clean the affected data
//! cache lines, then invalidate the corresponding instruction cache lines.
//!
//! [`sync_instruction`] performs exactly that for a virtual address range, using
//! the per-CPU cache line sizes reported by `CTR_EL0`. It is the counterpart to
//! the user-program loader in the kernel.

use core::arch::asm;

/// Make instructions written to `[start, start + len)` (via the data path)
/// coherent for execution: clean D-cache to PoU, then invalidate I-cache to PoU.
///
/// # Safety
/// `start`/`len` must describe a currently-mapped, readable range whose bytes
/// have already been written. Issues cache maintenance operations by VA, which
/// are valid at EL1.
pub unsafe fn sync_instruction(start: u64, len: usize) {
    if len == 0 {
        return;
    }
    let end = start + len as u64;

    // CTR_EL0 encodes the minimum cache line sizes in words (4 bytes):
    //   DminLine = bits [19:16], IminLine = bits [3:0]; size = 4 << field.
    let ctr: u64;
    // SAFETY: reading CTR_EL0 is permitted at EL1 and has no side effects.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags)) };
    let dline = 4u64 << ((ctr >> 16) & 0xf);
    let iline = 4u64 << (ctr & 0xf);

    // Clean the data cache by VA to the Point of Unification.
    let mut addr = start & !(dline - 1);
    while addr < end {
        // SAFETY: `dc cvau` by VA is valid at EL1; the address is in the range.
        unsafe { asm!("dc cvau, {}", in(reg) addr, options(nostack, preserves_flags)) };
        addr += dline;
    }
    // SAFETY: order the cleans before the I-cache invalidations.
    unsafe { asm!("dsb ish", options(nostack, preserves_flags)) };

    // Invalidate the instruction cache by VA to the Point of Unification.
    let mut addr = start & !(iline - 1);
    while addr < end {
        // SAFETY: `ic ivau` by VA is valid at EL1; the address is in the range.
        unsafe { asm!("ic ivau, {}", in(reg) addr, options(nostack, preserves_flags)) };
        addr += iline;
    }
    // SAFETY: complete the invalidations and flush the pipeline before the newly
    // written instructions can be fetched.
    unsafe {
        asm!("dsb ish", "isb", options(nostack, preserves_flags));
    }
}

/// Clean **and** invalidate the D-cache over `[start, start + len)` to the Point of
/// Coherency, for exchanging a buffer with a non-coherent DMA agent — e.g. the
/// Raspberry Pi VideoCore GPU, which reads a mailbox message and writes its reply
/// straight to RAM. Clean before handing the GPU the buffer so it sees our request;
/// invalidate after so we see its reply and not a stale cached copy. `dc civac`
/// does both per line, ordered by a full-system `dsb sy` (the agent is outside the
/// inner-shareable domain, so `ish` is not enough).
///
/// # Safety
/// `start`/`len` must describe a currently-mapped range. Cache maintenance by VA is
/// valid at EL1.
pub unsafe fn clean_invalidate_data(start: u64, len: usize) {
    if len == 0 {
        return;
    }
    let end = start + len as u64;
    let ctr: u64;
    // SAFETY: reading CTR_EL0 is permitted at EL1 and has no side effects.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags)) };
    let dline = 4u64 << ((ctr >> 16) & 0xf);

    let mut addr = start & !(dline - 1);
    while addr < end {
        // SAFETY: `dc civac` by VA is valid at EL1; the address is in the range.
        unsafe { asm!("dc civac, {}", in(reg) addr, options(nostack, preserves_flags)) };
        addr += dline;
    }
    // SAFETY: order the maintenance against the DMA agent's accesses (system scope).
    unsafe { asm!("dsb sy", options(nostack, preserves_flags)) };
}
