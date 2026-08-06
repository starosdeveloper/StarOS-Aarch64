//! MMU bring-up for EL1: the kernel's linear map in `TTBR1_EL1`.
//!
//! AArch64 gives EL1 *two* translation bases, selected by the top bits of the
//! virtual address: `TTBR0_EL1` for the bottom of the space and `TTBR1_EL1` for
//! the top. The kernel lives entirely in the top half. A user task owns the
//! bottom half outright — its `TTBR0` contains its pages and **nothing else**.
//!
//! That split is what a context switch depends on. Writing `TTBR0_EL1` changes
//! only the user half, so the kernel's code, stack, vectors and MMIO stay
//! mapped and reachable no matter whose address space is current — the EL1
//! handler for a fault taken from EL0 runs before anything is switched, and it
//! has to work. The alternative (which this kernel used to do) is to copy the
//! kernel's mappings into every task's tables and hope they never drift.
//!
//! # The linear map
//!
//! The upper half is one flat window onto physical memory:
//!
//! ```text
//! virtual  =  physical | KERNEL_VA_OFFSET          (for physical < 512 GiB)
//! ```
//!
//! so [`phys_to_virt`] is an OR and [`virt_to_phys`] is an AND — no lookup, no
//! table walk, callable from anywhere, including code that is about to *build*
//! a page table. Everything the kernel touches by physical address (a frame
//! from the allocator, a device the tree named, the blob itself) is reached
//! through it. One level-1 table of 512 one-gigabyte blocks covers the whole
//! window, which is why [`LINEAR_MAP_LIMIT`] is 512 GiB.
//!
//! The *contents* of that window are a runtime fact. [`init`] takes the RAM
//! window the device tree reported and marks RAM **Normal** write-back (the
//! kernel has to zero frames and walk tables at sane speed), everything below
//! it **Device** (where the UART and GIC sit on `virt` and on the phones this
//! targets), and everything above it *unmapped*, so a stray access faults
//! instead of quietly succeeding.
//!
//! Before the tree can be read, [`crate::boot`] has already built a provisional
//! version of this same table — see there for why it must, and what it assumes.

use core::arch::asm;
use core::cell::UnsafeCell;
use core::ptr::{read_volatile, write_volatile};

/// A translation table: 512 eight-byte descriptors, naturally 4 KiB aligned.
#[repr(C, align(4096))]
struct Table([u64; 512]);

/// Shareable wrapper so the tables can be `static`.
struct TableCell(UnsafeCell<Table>);
// SAFETY: single-core. Written by `_start` before translation is on and by
// `init` once, during early boot, before anything else can reach them.
unsafe impl Sync for TableCell {}

/// Level-0 table for `TTBR1_EL1`: one entry, pointing at [`BOOT_L1_HIGH`].
///
/// Filled by `_start`. `#[no_mangle]` because the boot stub addresses these by
/// symbol — they cannot be locals of a Rust function, since they must exist and
/// be writable before any Rust runs.
#[no_mangle]
#[link_section = ".bss.boot_tables"]
static BOOT_L0_HIGH: TableCell = TableCell(UnsafeCell::new(Table([0; 512])));

/// Level-1 table for `TTBR1_EL1`: the linear map itself, 512 × 1 GiB blocks.
/// `_start` fills it provisionally; [`init`] rewrites it once RAM is known.
#[no_mangle]
#[link_section = ".bss.boot_tables"]
static BOOT_L1_HIGH: TableCell = TableCell(UnsafeCell::new(Table([0; 512])));

/// Level-0 table of the throwaway identity map — see [`BOOT_L1_ID`].
#[no_mangle]
#[link_section = ".bss.boot_tables"]
static BOOT_L0_ID: TableCell = TableCell(UnsafeCell::new(Table([0; 512])));

/// Level-1 table of the throwaway identity map: it exists only for the instant
/// between enabling translation and branching to the kernel's real (high)
/// addresses, when the PC is still physical and that physical address has to
/// resolve to something. `_start` drops it from `TTBR0_EL1` immediately after.
#[no_mangle]
#[link_section = ".bss.boot_tables"]
static BOOT_L1_ID: TableCell = TableCell(UnsafeCell::new(Table([0; 512])));

// Descriptor bits (ARMv8-A VMSA, stage 1). Shared with `crate::addrspace`,
// which builds per-task tables from the same descriptor vocabulary.
const DESC_BLOCK: u64 = 0b01; // block entry at level 1/2
pub(crate) const DESC_TABLE: u64 = 0b11; // table entry (points to the next level)
const DESC_PAGE: u64 = 0b11; // page entry at level 3
const AF: u64 = 1 << 10; // Access Flag (else first access faults)
const SH_INNER: u64 = 0b11 << 8; // inner shareable
const AP_RW_EL1: u64 = 0b00 << 6; // read/write at EL1, no EL0 access
const AP_RW_EL0: u64 = 0b01 << 6; // read/write at EL1 and EL0
const AP_RO_EL0: u64 = 0b11 << 6; // read-only at EL1 and EL0 (no writes)
const PXN: u64 = 1 << 53; // privileged execute-never
const UXN: u64 = 1 << 54; // unprivileged execute-never
const ATTR_NORMAL: u64 = 0; // MAIR AttrIndx 0 (bits [4:2] = 0)
const ATTR_DEVICE: u64 = 1 << 2; // MAIR AttrIndx 1: Device-nGnRE
#[expect(dead_code, reason = "declared in MAIR for the drivers that will need it")]
const ATTR_DEVICE_STRONG: u64 = 2 << 2; // MAIR AttrIndx 2: Device-nGnRnE
const ATTR_NORMAL_NC: u64 = 3 << 2; // MAIR AttrIndx 3: Normal non-cacheable

/// Size of a 4 KiB page.
pub(crate) const PAGE_4K: u64 = 0x1000;
/// Size of a 1 GiB level-1 block.
const BLOCK_1G: u64 = 0x4000_0000;

/// Distance between a physical address and its address in the kernel's linear
/// map. The top 16 bits being all ones is what makes the CPU translate through
/// `TTBR1_EL1` rather than `TTBR0_EL1`.
///
/// **Must equal `KERNEL_VA_OFFSET` in `linker.ld`**: the boot stub builds the
/// map from this constant and then jumps to code linked against that one. They
/// are two spellings of a single number, in two languages that cannot share it.
pub const KERNEL_VA_OFFSET: u64 = 0xFFFF_0000_0000_0000;

/// How much physical address space the linear map reaches: one level-1 table of
/// 1 GiB blocks. Physical memory beyond this is invisible to the kernel.
pub const LINEAR_MAP_LIMIT: u64 = 512 * BLOCK_1G;

/// The kernel's virtual address for physical address `pa`.
///
/// Not a lookup — the linear map is an offset, so this is just where `pa` can be
/// reached. It says nothing about whether `pa` is *mapped*: an address above the
/// RAM window has no descriptor and faults when touched.
///
/// # Panics
/// Debug builds check `pa < LINEAR_MAP_LIMIT`; beyond that the OR would silently
/// alias some other physical address, which is worse than a panic.
#[inline]
#[must_use]
pub const fn phys_to_virt(pa: u64) -> u64 {
    debug_assert!(pa < LINEAR_MAP_LIMIT, "physical address outside the linear map");
    pa | KERNEL_VA_OFFSET
}

/// The physical address behind a kernel virtual address in the linear map.
///
/// Only valid for addresses in the linear map — which every kernel symbol is,
/// the kernel image being mapped there like everything else.
#[inline]
#[must_use]
pub const fn virt_to_phys(va: u64) -> u64 {
    debug_assert!(va >= KERNEL_VA_OFFSET, "virtual address outside the linear map");
    va & !KERNEL_VA_OFFSET
}

/// MAIR_EL1 — the four memory types this kernel can name.
///
/// A page's descriptor carries a 3-bit *index* into this register, not the
/// attributes themselves, so every type the kernel will ever use has to be
/// declared here at boot. QEMU has no real caches and treats most of these
/// alike; on hardware the difference between them is the difference between a
/// driver that works and one that hangs, so they are spelled out.
///
///  - **Attr0, Normal write-back** (`0xFF`): read/write-allocate, inner and outer.
///    Ordinary RAM — the kernel image, stacks, page tables, user pages.
///  - **Attr1, Device-nGnRE** (`0x04`): non-Gathering, non-Reordering, Early
///    write acknowledgement. This is what MMIO gets, and it is deliberately
///    *not* the strictest option: `nGnRE` is what Linux's `ioremap` uses for
///    ordinary device registers, and the early-ack is what keeps a driver from
///    stalling on every store to a FIFO.
///  - **Attr2, Device-nGnRnE** (`0x00`): as above but the write must be
///    acknowledged by the endpoint before the CPU moves on. The strictest type
///    there is. Reserved for the registers where a posted write would be a bug —
///    some power/reset controllers, and a few documented errata.
///  - **Attr3, Normal non-cacheable** (`0x44`): RAM the CPU must not cache
///    because something else — a DMA engine — is also looking at it. Without
///    this, "the device read stale data" is the bug, and it does not reproduce
///    under QEMU.
pub(crate) const MAIR: u64 = MAIR_NORMAL_WB
    | (MAIR_DEVICE_NGNRE << 8)
    | (MAIR_DEVICE_NGNRNE << 16)
    | (MAIR_NORMAL_NC << 24);

/// Normal memory, inner/outer write-back read/write-allocate.
const MAIR_NORMAL_WB: u64 = 0xFF;
/// Device, non-Gathering, non-Reordering, Early write acknowledgement.
const MAIR_DEVICE_NGNRE: u64 = 0x04;
/// Device, non-Gathering, non-Reordering, no Early write acknowledgement.
const MAIR_DEVICE_NGNRNE: u64 = 0x00;
/// Normal memory, inner/outer non-cacheable.
const MAIR_NORMAL_NC: u64 = 0x44;

/// TCR_EL1: both halves live, 48-bit each, 4 KiB granule, cacheable walks.
///
/// The trap here is `TG1`: it does **not** use the same encoding as `TG0`. For
/// `TG0`, 4 KiB is `0b00`; for `TG1` it is `0b10`, and `0b00` is reserved. A
/// zero left in `TG1` out of symmetry is a translation fault on the first
/// kernel instruction after the MMU comes on, with no way to print why.
pub(crate) const TCR: u64 = 16          // T0SZ = 16 (48-bit user VA)
    | (0b01 << 8)            // IRGN0 = WB WA
    | (0b01 << 10)           // ORGN0 = WB WA
    | (0b11 << 12)           // SH0   = inner shareable
    // TG0 = 0b00 (4 KiB granule) contributes nothing and is omitted.
    | (16 << 16)             // T1SZ = 16 (48-bit kernel VA)
    | (0b01 << 24)           // IRGN1 = WB WA
    | (0b01 << 26)           // ORGN1 = WB WA
    | (0b11 << 28)           // SH1   = inner shareable
    | (0b10 << 30); // TG1   = 4 KiB granule (not 0b00 — see above)
// IPS is deliberately absent: the boot stub reads the CPU's own PARange out of
// `ID_AA64MMFR0_EL1` and ORs it in. A guessed IPS is a guess about how much
// physical address space the chip has.

// SCTLR_EL1 enable bits.
const SCTLR_M: u64 = 1 << 0; // MMU enable
const SCTLR_C: u64 = 1 << 2; // data cache enable
const SCTLR_I: u64 = 1 << 12; // instruction cache enable

/// What `_start` sets in `SCTLR_EL1` to turn translation and caching on.
pub(crate) const SCTLR_ENABLE: u64 = SCTLR_M | SCTLR_C | SCTLR_I;

/// A 1 GiB linear-map block for Normal write-back memory (executable at EL1).
pub(crate) const fn normal_block(pa: u64) -> u64 {
    pa | DESC_BLOCK | AF | SH_INNER | AP_RW_EL1 | ATTR_NORMAL | UXN
}

/// A 1 GiB linear-map block for Device memory (never executable).
pub(crate) const fn device_block(pa: u64) -> u64 {
    pa | DESC_BLOCK | AF | AP_RW_EL1 | ATTR_DEVICE | PXN | UXN
}

/// The attribute bits of [`normal_block`], for the boot stub to OR onto a block
/// address in assembly.
pub(crate) const NORMAL_BLOCK_BITS: u64 = normal_block(0);
/// The attribute bits of [`device_block`], likewise.
pub(crate) const DEVICE_BLOCK_BITS: u64 = device_block(0);

/// A 4 KiB user *text* page: read + execute at EL0, **not** writable and never
/// executable at EL1. This is the W^X mapping the ELF loader gives a `PT_LOAD`
/// segment whose `p_flags` carry `PF_X` — code cannot be modified once loaded.
pub(crate) const fn user_text_page(pa: u64) -> u64 {
    pa | DESC_PAGE | AF | SH_INNER | AP_RO_EL0 | ATTR_NORMAL | PXN
}

/// A 4 KiB user *read-only data* page: readable at EL0, never writable or
/// executable. Used by the ELF loader for a `PT_LOAD` segment that is neither
/// writable (`PF_W`) nor executable (`PF_X`).
pub(crate) const fn user_ro_page(pa: u64) -> u64 {
    pa | DESC_PAGE | AF | SH_INNER | AP_RO_EL0 | ATTR_NORMAL | PXN | UXN
}

/// A 4 KiB user data/stack page: read/write at EL0, never executable.
pub(crate) const fn user_data_page(pa: u64) -> u64 {
    pa | DESC_PAGE | AF | SH_INNER | AP_RW_EL0 | ATTR_NORMAL | PXN | UXN
}

/// A 4 KiB user *device* page: read/write at EL0, Device memory, never
/// executable. This is what lets a user-space driver do its own MMIO — the whole
/// point of pushing drivers out of the kernel. No shareability bits: they are
/// ignored for Device memory (matching [`device_block`]).
pub(crate) const fn user_device_page(pa: u64) -> u64 {
    pa | DESC_PAGE | AF | AP_RW_EL0 | ATTR_DEVICE | PXN | UXN
}

/// A 4 KiB user *DMA* page: read/write at EL0, **Normal non-cacheable**, never
/// executable. Non-cacheable so a device writing straight to RAM and the CPU
/// reading the same buffer agree without explicit cache maintenance — the
/// simplest correct coherency for a DMA buffer. Shareability is architecturally
/// ignored for non-cacheable memory, so the inner-shareable bits carry no meaning
/// here and only match the other descriptors.
pub(crate) const fn user_dma_page(pa: u64) -> u64 {
    pa | DESC_PAGE | AF | SH_INNER | AP_RW_EL0 | ATTR_NORMAL_NC | PXN | UXN
}

/// Bit 0 of any descriptor: this entry means anything at all.
pub(crate) const DESC_VALID: u64 = 1 << 0;
/// The two-bit type field every descriptor starts with.
pub(crate) const DESC_KIND: u64 = 0b11;
/// The output address a descriptor carries, bits `[47:12]` — the next table, or
/// the frame itself at level 3.
pub(crate) const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

/// Marks a leaf whose frame belongs to the address space that mapped it, and is
/// therefore the space's to free when it is torn down.
///
/// Bits `[58:55]` are reserved for software, so the hardware ignores this and the
/// tables can carry their own ownership record. That is the point: an address
/// space's frame list used to be a fixed array beside the tables, which is a
/// second source of truth that can disagree with them and a hard cap on how many
/// pages a process may have. A bit *in* the descriptor cannot drift from the
/// mapping it describes, and there is no count to run out of.
///
/// A mapped MMIO page never gets this bit — the device is not ours to hand back
/// to the frame allocator — which is why teardown needs no special case for it.
pub(crate) const SW_OWNED: u64 = 1 << 55;

/// The access-permission field of a leaf descriptor, bits `[7:6]`.
const AP_MASK: u64 = 0b11 << 6;

/// Whether EL0 may read through this leaf descriptor.
///
/// Both EL0-accessible encodings (`AP_RW_EL0`, `AP_RO_EL0`) have bit 6 set; the
/// EL1-only ones do not.
pub(crate) const fn user_readable(desc: u64) -> bool {
    desc & DESC_VALID != 0 && desc & (1 << 6) != 0
}

/// Whether EL0 may *write* through this leaf descriptor.
///
/// Only `AP_RW_EL0` qualifies. Worth checking even for a kernel access: `AP_RO_EL0`
/// is read-only at EL1 *too*, so writing a user's text page on its behalf would
/// fault the kernel rather than quietly succeed.
pub(crate) const fn user_writable(desc: u64) -> bool {
    desc & DESC_VALID != 0 && desc & AP_MASK == AP_RW_EL0
}

/// How many bits of physical address this CPU implements, from
/// `ID_AA64MMFR0_EL1.PARange`.
///
/// The boot stub programs `TCR_EL1.IPS` from this same field rather than a
/// constant — a guessed IPS is a guess about the chip. Reported at boot so the
/// guess-free path is visible rather than merely claimed.
#[must_use]
pub fn pa_bits() -> u32 {
    let mmfr0: u64;
    // SAFETY: reading ID_AA64MMFR0_EL1 is permitted at EL1 and side-effect free.
    unsafe {
        asm!("mrs {v}, id_aa64mmfr0_el1", v = out(reg) mmfr0, options(nomem, nostack, preserves_flags));
    }
    // PARange encoding, shared with TCR_EL1.IPS.
    match mmfr0 & 0xf {
        0 => 32,
        1 => 36,
        2 => 40,
        3 => 42,
        4 => 44,
        5 => 48,
        6 => 52,
        _ => 56,
    }
}

/// The RAM window this kernel is running in, as the device tree reported it.
struct RamWindow(UnsafeCell<(u64, u64)>);
// SAFETY: written once by `init` during single-core early boot, read-only after.
unsafe impl Sync for RamWindow {}
static RAM_WINDOW: RamWindow = RamWindow(UnsafeCell::new((0, 0)));

/// The RAM window `[start, end)` the kernel was told about.
#[must_use]
pub fn ram_window() -> (u64, u64) {
    // SAFETY: only mutated by `init` before any of this is reachable.
    unsafe { *RAM_WINDOW.0.get() }
}

/// The descriptor the linear map should hold for the 1 GiB block at `pa`.
const fn linear_entry(pa: u64, ram_start: u64, ram_end: u64) -> u64 {
    if pa >= ram_start && pa < ram_end {
        // RAM: cacheable, so the kernel can zero frames and walk tables at sane
        // speed — and so it can execute, which Device memory does not allow.
        normal_block(pa)
    } else if pa < ram_start {
        // Below RAM is where MMIO lives on every machine this targets.
        device_block(pa)
    } else {
        // Above RAM: nothing we know of. Leave it unmapped so a stray access
        // faults rather than quietly working.
        0
    }
}

/// Complete all outstanding table writes and drop every cached translation.
///
/// # Safety
/// Call at EL1. Every mapping the caller still needs must be live in the tables
/// by the time this returns.
unsafe fn flush_translations() {
    // SAFETY: maintenance operations, all valid at EL1.
    unsafe {
        asm!(
            "dsb ish",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags),
        );
    }
}

/// Replace the boot stub's provisional linear map with the real one, now that
/// the device tree has said where RAM is.
///
/// Translation is **already on** when this runs — the stub turned it on before
/// any Rust could execute — so this rewrites a live table, and does it
/// break-before-make: every descriptor whose meaning changes is invalidated,
/// the TLBs are dropped, and only then are the new values written. Changing a
/// live block's memory type in place is architecturally undefined; the CPU is
/// entitled to hold both old and new attributes at once.
///
/// This is safe to do while running out of the very table being rewritten
/// because the entries the kernel is *using* do not change: the stub already
/// mapped the block holding the kernel (and the blob) as Normal, which is what
/// RAM gets here, and everything below RAM was Device and stays Device. Only
/// blocks nothing has touched yet are disturbed.
///
/// # Safety
/// Call once, at EL1, from code running in the linear map. `[ram_start,
/// ram_end)` must be the machine's actual RAM: mapping RAM that does not exist,
/// or failing to map RAM that does, both fault later rather than here.
pub unsafe fn init(ram_start: u64, ram_end: u64) {
    // SAFETY: single-core early boot; this is the only writer of the window.
    unsafe { *RAM_WINDOW.0.get() = (ram_start, ram_end) };

    // The table is a kernel static, so this *is* its linear-map address.
    let l1 = BOOT_L1_HIGH.0.get().cast::<u64>();

    let mut changed = false;
    for i in 0..512usize {
        let want = linear_entry(i as u64 * BLOCK_1G, ram_start, ram_end);
        // SAFETY: `i < 512`, so this stays inside the table we own.
        let have = unsafe { read_volatile(l1.add(i)) };
        if want != have {
            // SAFETY: as above. Break: the old translation must be gone before
            // a new one with different attributes can be published.
            unsafe { write_volatile(l1.add(i), 0) };
            changed = true;
        }
    }
    if changed {
        // SAFETY: the kernel's own block was not among the invalidated entries
        // (its descriptor is unchanged), so we can still fetch instructions.
        unsafe { flush_translations() };
    }

    for i in 0..512usize {
        let want = linear_entry(i as u64 * BLOCK_1G, ram_start, ram_end);
        // SAFETY: `i < 512`. Make: publish the new descriptors.
        unsafe { write_volatile(l1.add(i), want) };
    }
    // SAFETY: nothing above needed the invalidated entries; make the new ones
    // visible to the table walker.
    unsafe { flush_translations() };
}

/// Map the 1 GiB linear-map block containing physical `pa` as Device memory, if it
/// is not already mapped that way.
///
/// [`init`] maps only RAM (Normal) and the below-RAM device region, leaving
/// everything *above* RAM unmapped so strays fault. But some MMIO legitimately
/// lives up there — a PCIe ECAM config window the tree places past the DRAM banks,
/// for one — and must be reachable through [`phys_to_virt`] to be driven. This adds
/// that one block. Granularity is a whole gigabyte, which is fine for Device memory
/// (no speculation, so mapping a little extra address space is harmless).
///
/// # Safety
/// Call at EL1 after [`init`]. `pa` must name real MMIO for this machine — mapping a
/// RAM address this way would wrongly re-type it as Device.
pub unsafe fn map_device_block(pa: u64) {
    assert!(pa < LINEAR_MAP_LIMIT, "device address past the linear map");
    let idx = (pa / BLOCK_1G) as usize;
    let want = device_block(idx as u64 * BLOCK_1G);
    let l1 = BOOT_L1_HIGH.0.get().cast::<u64>();
    // SAFETY: `idx < 512` (checked via LINEAR_MAP_LIMIT). Going from unmapped (0) to
    // a valid Device block needs no break-before-make; publish and flush.
    unsafe {
        if read_volatile(l1.add(idx)) != want {
            write_volatile(l1.add(idx), want);
            flush_translations();
        }
    }
}

/// Read the currently active low-half translation base (`TTBR0_EL1`).
///
/// Used by the scheduler to capture the base it must restore when switching
/// back to a context with no user address space of its own. Since the kernel
/// moved to `TTBR1`, that base is simply zero: the bootstrap context maps no
/// user memory at all, and never touches a low address.
#[must_use]
pub fn ttbr0() -> u64 {
    let ttbr0: u64;
    // SAFETY: reading TTBR0_EL1 is permitted at EL1 and side-effect free.
    unsafe {
        asm!("mrs {t}, ttbr0_el1", t = out(reg) ttbr0, options(nomem, nostack, preserves_flags));
    }
    ttbr0
}

/// Switch the active user address space to `ttbr0` and drop stale translations.
///
/// Safe to execute from kernel code because the kernel is not in `TTBR0`: this
/// changes only the bottom half of the address space, and the code doing the
/// switching lives in the top. The TLB flush is required because every space
/// uses ASID 0, so the same user VA would otherwise still resolve to the
/// previous task's frame.
///
/// # Safety
/// `ttbr0` must be the physical address of a well-formed level-0 table, or zero
/// for "no user space". Call only at EL1.
pub unsafe fn set_ttbr0(ttbr0: u64) {
    // SAFETY: install the new base, then invalidate all EL1&0 TLB entries so no
    // stale user translation survives the switch. Barriers order the table
    // publication (already done by the builder) against the walk.
    //
    // The invalidate is deliberately **local** (`vmalle1`, not `vmalle1is`): a
    // TTBR0 switch changes only *this* core's user translations, so broadcasting it
    // to every core — which the Inner Shareable form does — needlessly flushes the
    // live user TLB of whatever the other cores are running, on *every* context
    // switch. Cross-core coherence is still guaranteed where it actually matters:
    // when an address space is torn down (`addrspace::destroy`) or its mappings
    // change (`map_anon`), those paths issue the broadcast `...is` invalidate, and a
    // core switching *into* a space always runs this local invalidate first.
    unsafe {
        asm!(
            "dsb ish",
            "msr ttbr0_el1, {t}",
            "isb",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            t = in(reg) ttbr0,
            options(nostack, preserves_flags),
        );
    }
}

/// Returns `true` if the MMU is currently enabled (`SCTLR_EL1.M`).
#[must_use]
pub fn is_enabled() -> bool {
    let sctlr: u64;
    // SAFETY: reading SCTLR_EL1 is permitted at EL1 and side-effect free.
    unsafe {
        asm!("mrs {s}, sctlr_el1", s = out(reg) sctlr, options(nomem, nostack, preserves_flags));
    }
    sctlr & SCTLR_M != 0
}
