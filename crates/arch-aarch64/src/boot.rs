//! Reset vector and early boot.
//!
//! We are entered at `_start` on the primary core with the MMU off. Two things
//! have to happen before Rust can run at all, and they are the whole reason this
//! file is assembly.
//!
//! **The boot contract is the ARM64 Linux one**, which every real bootloader
//! (and QEMU's boot stub) implements: `x0` holds the physical address of a
//! flattened device tree describing the machine, `x1`..`x3` are reserved, and
//! entry is at **EL2 if the hardware has it**, EL1 otherwise — Linux asks for EL2
//! so that KVM can use it. This is an EL1 kernel, so the stub drops down when it
//! finds itself upstairs; that is the only reason any of it knows EL2 exists.
//! That pointer is the kernel's only input, and it is the difference between a
//! kernel that knows one machine and a kernel that can be told about any
//! machine — so the stub's job includes *not losing it*: `x0` is parked in `x20`
//! across everything below and handed to `rust_start` at the end.
//!
//! **The kernel is linked high and loaded low.** It runs out of `TTBR1_EL1` at
//! `KERNEL_VA_OFFSET + physical` (see [`crate::mmu`]), but a bootloader knows
//! nothing of that: it drops the image at a physical address and branches to its
//! first byte with translation off, so the PC — and every address the CPU can
//! form — is physical. Every high address baked into the image is unusable until
//! the MMU is on. So this stub cannot call Rust first and set up paging later;
//! it must build the tables itself, out of instructions that never depend on
//! being at their link address:
//!
//!  - `adrp`/`add` are PC-relative, so with the MMU off they hand back a
//!    symbol's *physical* address (the image is a constant offset from its link
//!    address, and that offset cancels). This is how `.bss` gets zeroed and how
//!    the tables get found.
//!  - `ldr xN, =symbol` loads an absolute address out of a literal pool. The
//!    *load* is PC-relative, so it works either way; the *value* is a link-time
//!    high address, so it is only useful once translation is on. That is exactly
//!    what makes it the right instruction for the jump upstairs.
//!
//! # What the provisional map assumes
//!
//! The tables built here have to be good enough to run the FDT parser, because
//! the device tree is what tells us where RAM is — the map cannot wait for
//! information that can only be read through the map. So it is built from what
//! is knowable without asking anyone:
//!
//!  - Every 1 GiB block of the low 512 GiB is mapped **Device**. Device memory
//!    permits no speculation, so mapping address space we know nothing about is
//!    harmless — and it means the UART answers at `phys_to_virt(base)` the
//!    moment we are high, with no console-shaped chicken-and-egg problem.
//!  - The block containing **the kernel** is instead **Normal**: instruction
//!    fetch from Device memory is not architecturally allowed, and `.data`,
//!    `.bss` and the stack want caching.
//!  - The block containing **the device tree blob** is Normal too. It is RAM by
//!    definition, and usually the same block as the kernel — but "usually" is
//!    not something a boot path should rely on.
//!
//! [`mmu::init`](crate::mmu::init) replaces all of this with the real map as
//! soon as the tree has been read. It relies on the blocks listed above keeping
//! the descriptors they have here, so that the rewrite never disturbs a mapping
//! that is in use.

use core::arch::{asm, global_asm};
use core::cell::UnsafeCell;

use crate::mmu::{DEVICE_BLOCK_BITS, MAIR, NORMAL_BLOCK_BITS, SCTLR_ENABLE, TCR};

// The kernel binary provides the real entry points. Declaring them here (rather
// than depending on the kernel crate, which would be circular) lets the arch
// crate own the boot code while the kernel owns policy. The symbols are resolved
// at final link time.
extern "Rust" {
    fn kmain(dtb: u64) -> !;
    fn ksecondary(cpu: u64) -> !;
}

/// Most cores this kernel will schedule on. A phone has eight; the array of
/// secondary stacks below is sized from this, so it is not free to grow.
pub const MAX_CPUS: usize = 8;

/// Stack for one secondary core. The same size as the primary's `.stack`: the
/// kernel does not know or care which core it is running on, so a secondary
/// needs exactly as much room as the primary.
const SECONDARY_STACK_SIZE: usize = 0x10000;

#[repr(C, align(16))]
struct CpuStack(UnsafeCell<[u8; SECONDARY_STACK_SIZE]>);
// SAFETY: each core touches only its own element, and only as its stack — which
// is by definition private to it from its first instruction onwards.
unsafe impl Sync for CpuStack {}

/// One stack per core, in `.bss` (so it costs nothing in the image). Slot 0 is
/// never used — the primary is already running on the linker's `.stack` by the
/// time anything here is readable — but keeping the indices aligned with cpu ids
/// is worth one unused slot.
static SECONDARY_STACKS: [CpuStack; MAX_CPUS] =
    [const { CpuStack(UnsafeCell::new([0; SECONDARY_STACK_SIZE])) }; MAX_CPUS];

/// Where each core's stack starts, indexed by cpu id — the array
/// `_secondary_start` reads to find its own, before it can call anything.
struct StackTops(UnsafeCell<[u64; MAX_CPUS]>);
// SAFETY: written by the primary, one slot per core, strictly before that core
// is asked to start; each core then reads only its own slot.
unsafe impl Sync for StackTops {}
#[no_mangle]
static SECONDARY_STACK_TOPS: StackTops = StackTops(UnsafeCell::new([0; MAX_CPUS]));

/// The **physical** address firmware must be told to start a secondary core at.
///
/// Physical because the waking core has its MMU off: it does not know the kernel
/// is linked high, and a virtual address would mean nothing to it.
#[must_use]
pub fn secondary_entry() -> u64 {
    extern "C" {
        static _secondary_start: u8;
    }
    crate::mmu::virt_to_phys(&raw const _secondary_start as u64)
}

/// Give cpu `cpu` a stack, and return the context id to hand PSCI for it.
///
/// Must be called before asking firmware to start that core: the core reads this
/// slot before it can execute anything that could have filled it in.
///
/// # Safety
/// `cpu` must be `< MAX_CPUS` and must not already be running.
pub unsafe fn arm_secondary(cpu: usize) -> Option<u64> {
    if cpu == 0 || cpu >= MAX_CPUS {
        return None;
    }
    let top = SECONDARY_STACKS[cpu].0.get() as u64 + SECONDARY_STACK_SIZE as u64;
    // SAFETY: the caller guarantees this core is not running, so nothing else can
    // be reading this slot; the write is published by the `dsb` below and, more
    // decisively, by the PSCI call the caller makes next.
    unsafe { (*SECONDARY_STACK_TOPS.0.get())[cpu] = top };
    // SAFETY: make the stack pointer visible to a core that has not started yet.
    unsafe { asm!("dsb ish", options(nostack, preserves_flags)) };
    Some(cpu as u64)
}

/// This core's id, as recorded in `TPIDR_EL1` when it came up.
///
/// `TPIDR_EL1` is the architecture's per-core scratch register: it is banked per
/// core by the hardware, so it answers "who am I" with no lock, no lookup and no
/// dependence on any kernel structure — which is what makes it usable from the
/// places that are about to *take* a lock.
#[must_use]
pub fn cpu_id() -> u64 {
    let id: u64;
    // SAFETY: reading TPIDR_EL1 is permitted at EL1 and side-effect free.
    unsafe { asm!("mrs {id}, tpidr_el1", id = out(reg) id, options(nomem, nostack, preserves_flags)) };
    id
}

/// Record this core's id in `TPIDR_EL1`.
///
/// # Safety
/// Call once per core, during its bring-up, before anything asks [`cpu_id`].
unsafe fn set_cpu_id(id: u64) {
    // SAFETY: writing TPIDR_EL1 is permitted at EL1; it is per-core scratch with
    // no architectural meaning of its own.
    unsafe { asm!("msr tpidr_el1, {id}", id = in(reg) id, options(nomem, nostack, preserves_flags)) };
}

global_asm!(
    r#"
.section ".text.boot", "ax"
.global _start
_start:
    // ---- ARM64 Linux image header (Documentation/arch/arm64/booting.rst) ----
    //
    // This 64-byte header is what makes the image loadable by a *bootloader*
    // rather than only by QEMU's ELF loader. An Android bootloader — and QEMU's
    // own boot stub, which is what hands us the device tree — parses this and
    // nothing else; without it we are just an ELF that nothing on a real device
    // knows how to start.
    //
    // The first word must itself be a valid instruction (an EFI-bootable kernel
    // puts "MZ" here); we simply branch over the header.
    b       .Lprimary               // 0x00  code0
    .long   0                       // 0x04  code1
    .quad   0x80000                 // 0x08  text_offset: load 512 KiB above the
                                    //       2 MiB-aligned base of DRAM, matching
                                    //       KERNEL_PHYS_BASE in linker.ld
    .quad   __image_size            // 0x10  image_size: bytes the loader must
                                    //       keep clear for us — .bss and the
                                    //       stack included, so nothing gets
                                    //       placed on top of them
    .quad   0                       // 0x18  flags: little-endian, 4 KiB pages,
                                    //       place near the base of DRAM
    .quad   0                       // 0x20  res2
    .quad   0                       // 0x28  res3
    .quad   0                       // 0x30  res4
    .ascii  "ARM\x64"               // 0x38  magic
    .long   0                       // 0x3c  res5 (PE header offset; unused)

.Lprimary:
    // x0 = physical address of the device tree blob, per the boot contract.
    // Park it somewhere nothing below touches; it is the one thing we cannot
    // recompute if we lose it.
    mov     x20, x0

    // Park every core except the primary (Aff0 == 0) in a wait loop. Secondaries
    // are not started by falling into the kernel — firmware holds them in reset
    // and we ask for them later, by name, through PSCI (see `_secondary_start`).
    mrs     x1, mpidr_el1
    and     x1, x1, #0xFF
    cbnz    x1, .Lpark

    bl      .Ldrop_to_el1           // returns here, at EL1, with x21 = entry level

    // ---- Zero .bss ------------------------------------------------------
    // Before the tables, because the tables *are* .bss: they must start out as
    // 512 invalid descriptors, and only the entries filled below may be live.
    adrp    x1, __bss_start
    add     x1, x1, :lo12:__bss_start
    adrp    x2, __bss_end
    add     x2, x2, :lo12:__bss_end
.Lbss_zero:
    cmp     x1, x2
    b.hs    .Lbss_done
    str     xzr, [x1], #8
    b       .Lbss_zero
.Lbss_done:

    // ---- Build the provisional translation tables ------------------------
    // Our own physical base, straight out of the PC, and the 1 GiB block that
    // contains it.
    adrp    x1, _start              // physical base of the image
    lsr     x2, x1, #30             // kernel's block index
    lsl     x3, x2, #30             // kernel's block base address

    // TTBR1 level 1: 512 Device blocks covering the low 512 GiB of physical
    // address space, so the linear map answers everywhere from the start.
    adrp    x4, BOOT_L1_HIGH
    add     x4, x4, :lo12:BOOT_L1_HIGH
    ldr     x5, ={device_bits}
    mov     x6, xzr                 // block address
    mov     x7, xzr                 // index
    mov     x8, #0x40000000         // 1 GiB stride
.Lmap_device:
    orr     x9, x6, x5
    str     x9, [x4, x7, lsl #3]
    add     x6, x6, x8
    add     x7, x7, #1
    cmp     x7, #512
    b.lo    .Lmap_device

    // The kernel's own block: Normal, or nothing here executes.
    ldr     x5, ={normal_bits}
    orr     x9, x3, x5
    str     x9, [x4, x2, lsl #3]

    // The device tree's block: Normal, so the parser can read the blob. x20 is
    // whatever the bootloader left us, so both checks are load-bearing — a bad
    // index would write past the end of the table.
    cbz     x20, .Lno_dtb
    lsr     x6, x20, #30
    cmp     x6, #512
    b.hs    .Lno_dtb
    lsl     x7, x6, #30
    orr     x9, x7, x5
    str     x9, [x4, x6, lsl #3]
.Lno_dtb:

    // TTBR1 level 0: one entry, covering the whole linear map.
    adrp    x6, BOOT_L0_HIGH
    add     x6, x6, :lo12:BOOT_L0_HIGH
    orr     x9, x4, #3              // DESC_TABLE
    str     x9, [x6]

    // TTBR0 identity: one block, ours. It exists only to keep the PC valid for
    // the handful of instructions between switching translation on and branching
    // to the address the kernel is actually linked at.
    adrp    x7, BOOT_L1_ID
    add     x7, x7, :lo12:BOOT_L1_ID
    orr     x9, x3, x5              // same Normal block, at its physical address
    str     x9, [x7, x2, lsl #3]
    adrp    x8, BOOT_L0_ID
    add     x8, x8, :lo12:BOOT_L0_ID
    orr     x9, x7, #3              // DESC_TABLE
    str     x9, [x8]

    bl      .Lenable_mmu

    // Translated — but the PC is still physical, running out of the identity
    // map. This is the only instruction pair in the kernel that needs an
    // absolute address: `br` to where we are linked.
    ldr     x9, =.Lhigh
    br      x9

.Lhigh:
    // Upstairs. The stack is a linker symbol, so it is a high address and could
    // not have been installed before now.
    ldr     x9, =__stack_top
    mov     sp, x9

    // The identity map has done its job. Drop it, so that a stray low address —
    // a physical pointer used where a virtual one was meant — faults loudly
    // instead of quietly resolving to the right memory for the wrong reason.
    msr     ttbr0_el1, xzr
    dsb     ish
    tlbi    vmalle1
    dsb     ish
    isb

    // Enable FP/SIMD access at EL1 and EL0 (CPACR_EL1.FPEN = 0b11). Without this
    // any NEON/floating-point instruction the compiler emits — e.g. the
    // vectorised struct copies the kernel uses — traps as EC 0x07. Do it
    // before entering Rust so all Rust code may use the vector registers.
    mrs     x9, cpacr_el1
    orr     x9, x9, #(3 << 20)
    msr     cpacr_el1, x9
    isb

    // Hand the device tree pointer and the level we were entered at over to
    // rust_start. Both are things only this stub could have known.
    mov     x0, x20
    mov     x1, x21
    bl      rust_start

// ---------------------------------------------------------------------------
// Secondary cores
// ---------------------------------------------------------------------------
//
// A secondary does not fall into the kernel — firmware holds it in reset until
// the primary asks for it by name through PSCI `CPU_ON`, which is why this is a
// separate entry point rather than a branch in `_start`. It arrives exactly as
// the primary did: MMU off, PC physical, possibly at EL2. The difference is that
// there is nothing left to *build* — the primary's tables are still sitting in
// `.bss`, identity map included (the primary only stopped *using* TTBR0; it
// never tore the tables down, precisely so this path can borrow them).
.global _secondary_start
_secondary_start:
    // PSCI hands our context id back to us in x0: our index in the kernel's cpu
    // table, which is how we find our own stack a few instructions from now.
    mov     x20, x0
    bl      .Ldrop_to_el1
    bl      .Lenable_mmu
    ldr     x9, =.Lsec_high
    br      x9

.Lsec_high:
    // Our own stack. The primary filled this array in before it asked firmware
    // to start us, so the entry is already there — and it is a high address, so
    // it could not have been read a moment ago.
    ldr     x9, =SECONDARY_STACK_TOPS
    ldr     x9, [x9, x20, lsl #3]
    mov     sp, x9

    // Drop the identity map, same as the primary and for the same reason.
    msr     ttbr0_el1, xzr
    dsb     ish
    tlbi    vmalle1
    dsb     ish
    isb

    mrs     x9, cpacr_el1
    orr     x9, x9, #(3 << 20)
    msr     cpacr_el1, x9
    isb

    mov     x0, x20
    bl      rust_secondary_start

// ---------------------------------------------------------------------------
// Shared subroutines. Both entry points need these, and neither has a stack yet,
// so they are `bl`/`ret` through x30 and touch no memory.
// ---------------------------------------------------------------------------

// Drop to EL1 if we were entered at EL2, and leave the level we came in at in
// x21. Clobbers x1.
//
// The trick that makes this callable: when it does drop a level, it `eret`s to
// x30 — the address `bl` would have returned to anyway. So the caller resumes at
// its next instruction either way, and only x21 says which happened.
.Ldrop_to_el1:
    // A real bootloader hands off at EL2, because Linux wants EL2 for KVM. This
    // kernel is an EL1 kernel: its vectors are VBAR_EL1, its translation is
    // TTBR*_EL1, its timer is the EL1 physical timer. Left at EL2, none of that
    // is even wrong — it simply governs a translation regime we are not running
    // in, so the MMU never comes on and the branch to our link address lands in
    // unmapped space, before there is any console to say so.
    mrs     x21, CurrentEL
    lsr     x21, x21, #2
    cmp     x21, #2
    b.ne    .Ldrop_done             // already EL1: an ordinary return

    // EL1 is AArch64, and every other bit of HCR_EL2 is cleared. That includes
    // E2H and TGE: we want an ordinary EL1 kernel under a dormant EL2, not a VHE
    // host running at EL2 with EL1 registers redirected under us.
    mov     x1, #(1 << 31)          // HCR_EL2.RW
    msr     hcr_el2, x1
    isb

    // Our timer is the *physical* one (CNTP_*, see `timer.rs`), which EL1 may not
    // touch unless EL2 says so — CNTHCTL_EL2.EL1PCTEN and .EL1PCEN. Without this
    // the first timer access traps to EL2 instead of arming preemption.
    mrs     x1, cnthctl_el2
    orr     x1, x1, #3
    msr     cnthctl_el2, x1
    msr     cntvoff_el2, xzr        // no virtual offset: EL1's view is the real one

    // No stage-2 translation: EL1's page tables are the whole story.
    msr     vttbr_el2, xzr

    // Do not trap EL1/EL0 floating point to EL2 (CPTR_EL2.TFP = 0); the rest of
    // the register is RES1. The kernel enables FP for itself in CPACR_EL1, which
    // EL2 would otherwise override.
    mov     x1, #0x33ff
    msr     cptr_el2, x1

    // SCTLR_EL1 has an UNKNOWN reset value when we arrive at EL2, so give EL1 a
    // defined one: MMU and caches off, architectural RES1 bits set. The caller
    // turns translation on deliberately, from a known state.
    mov     x1, #0x0800
    movk    x1, #0x30d0, lsl #16
    msr     sctlr_el1, x1

    mov     x1, #0x3c5              // DAIF masked, M[3:0] = 0b0101 (EL1h)
    msr     spsr_el2, x1
    msr     elr_el2, x30            // land where `bl` would have returned
    eret
.Ldrop_done:
    ret

// Program the EL1 translation regime from the tables in `.bss` and switch
// translation on. Returns with the MMU on but the PC still physical, running out
// of the identity map — the caller branches to its link address itself, because
// only the caller knows where it is going. Clobbers x6, x8, x9, x10, x11.
//
// `ret` across the transition is safe for the same reason the `br` after it is
// needed: x30 holds a physical address, and the identity map covers the kernel's
// own block.
.Lenable_mmu:
    // The tables were written with the D-cache off, so they are in memory. But a
    // stale line for those addresses may still be sitting in the cache from
    // whatever ran before us, and the moment translation comes on the table
    // walker reads them *through* the cache (TCR asks for cacheable walks). A
    // dirty stale line evicting later would rewrite our tables underneath us.
    // Invalidate — not clean: memory holds the truth, the cache does not.
    adrp    x6, __boot_tables_start
    add     x6, x6, :lo12:__boot_tables_start
    adrp    x8, __boot_tables_end
    add     x8, x8, :lo12:__boot_tables_end
    mrs     x9, ctr_el0
    ubfx    x9, x9, #16, #4         // CTR_EL0.DminLine: log2(words per line)
    mov     x10, #4
    lsl     x9, x10, x9             // x9 = D-cache line size in bytes
    sub     x10, x9, #1
    bic     x6, x6, x10             // round the start down to a line boundary
.Linval_tables:
    dc      ivac, x6
    add     x6, x6, x9
    cmp     x6, x8
    b.lo    .Linval_tables
    dsb     sy

    adrp    x6, BOOT_L0_HIGH
    add     x6, x6, :lo12:BOOT_L0_HIGH
    adrp    x8, BOOT_L0_ID
    add     x8, x8, :lo12:BOOT_L0_ID
    ldr     x9, ={mair}
    msr     mair_el1, x9

    // TCR's IPS is the one field we must not invent: it says how much physical
    // address space translation may produce, and the answer belongs to the chip.
    // ID_AA64MMFR0_EL1.PARange uses the same encoding as TCR_EL1.IPS, so it maps
    // straight across — capped at 48 bits, which is all a 4 KiB granule can reach
    // without FEAT_LPA and far beyond the 512 GiB this kernel's linear map covers.
    ldr     x9, ={tcr}
    mrs     x10, id_aa64mmfr0_el1
    and     x10, x10, #0xf          // PARange
    mov     x11, #5                 // 5 = 48-bit
    cmp     x10, x11
    csel    x10, x10, x11, lo       // x10 = min(PARange, 48-bit)
    orr     x9, x9, x10, lsl #32    // TCR_EL1.IPS is bits [34:32]
    msr     tcr_el1, x9
    msr     ttbr0_el1, x8
    msr     ttbr1_el1, x6
    dsb     ish
    isb
    tlbi    vmalle1
    dsb     ish
    isb
    mrs     x9, sctlr_el1
    ldr     x10, ={sctlr}
    orr     x9, x9, x10
    msr     sctlr_el1, x9
    isb
    ret

.Lpark:
    wfe
    b       .Lpark
"#,
    device_bits = const DEVICE_BLOCK_BITS,
    normal_bits = const NORMAL_BLOCK_BITS,
    mair = const MAIR,
    tcr = const TCR,
    sctlr = const SCTLR_ENABLE,
);

/// The exception level the bootloader entered us at, recorded by `_start` before
/// it dropped to EL1.
struct EntryEl(UnsafeCell<u64>);
// SAFETY: written once by `rust_start` on the primary core before anything else
// runs, read-only afterwards.
unsafe impl Sync for EntryEl {}
static ENTRY_EL: EntryEl = EntryEl(UnsafeCell::new(0));

/// The exception level the bootloader handed control over at — 2 on hardware
/// with virtualisation, 1 without.
///
/// The kernel always *runs* at EL1; this is the one piece of the boot handoff
/// that cannot be recovered later, because by the time anything can ask,
/// `CurrentEL` reads 1 either way.
#[must_use]
pub fn entry_el() -> u64 {
    // SAFETY: only written by `rust_start`, before the kernel is reachable.
    unsafe { *ENTRY_EL.0.get() }
}

/// First Rust code to run after the assembly stub. Hands off to the kernel,
/// forwarding the device tree pointer the bootloader left in `x0`.
///
/// # Safety
/// Called exactly once by `_start`, at EL1, with translation already on, the
/// kernel running at its link addresses, a valid stack and a zeroed `.bss`.
/// `dtb` is a *physical* address — whatever the bootloader passed — and is not
/// trusted here; the kernel validates it before use. `entry_el` is the level
/// `_start` was entered at.
#[no_mangle]
pub unsafe extern "C" fn rust_start(dtb: u64, entry_el: u64) -> ! {
    // SAFETY: single-core early boot, before `.bss` is read by anything else;
    // this is the only writer.
    unsafe { *ENTRY_EL.0.get() = entry_el };
    // SAFETY: we are the primary core, by definition cpu 0, and nothing has asked
    // yet.
    unsafe { set_cpu_id(0) };
    // SAFETY: `kmain` is provided by the linked kernel binary and is designed
    // to be the single entry into kernel initialisation. It never returns.
    unsafe { kmain(dtb) }
}

/// First Rust code to run on a secondary core, once `_secondary_start` has
/// brought it to the same state the primary reached: EL1, translation on through
/// the primary's tables, running at its link addresses, on its own stack.
///
/// # Safety
/// Called exactly once per secondary core, by `_secondary_start`, with `cpu` the
/// context id the primary passed to PSCI — this core's index.
#[no_mangle]
pub unsafe extern "C" fn rust_secondary_start(cpu: u64) -> ! {
    // Before anything else: this core must be able to say who it is, because
    // everything it touches from here is shared with the other cores.
    // SAFETY: our own core, during our own bring-up, before anything asks.
    unsafe { set_cpu_id(cpu) };
    // SAFETY: `ksecondary` is provided by the linked kernel binary; it never
    // returns.
    unsafe { ksecondary(cpu) }
}
