//! Per-task address spaces built from a [`FrameAllocator`].
//!
//! Each user process gets its own set of translation tables — its own
//! `TTBR0_EL1` value — so the *same* user virtual address maps to a *different*
//! physical frame in each process. That is what makes two EL0 tasks isolated
//! from each other, not just from the kernel.
//!
//! **An address space contains the task's pages and nothing else.** No kernel
//! mappings are copied in: the kernel lives in `TTBR1_EL1` (see [`crate::mmu`]),
//! which a `TTBR0` switch does not touch, so the EL1 handler for an exception
//! taken from EL0 keeps its code, stack, vectors and MMIO no matter whose space
//! is current. That is the whole reason the kernel moved upstairs — while it sat
//! in `TTBR0` alongside user space, every task's tables had to carry a copy of
//! the kernel map, and every gigabyte of virtual address space this window
//! occupies cost a gigabyte of *physical* memory the kernel could not reach.
//!
//! The tables and the user code/data/stack frames are carved from a physical
//! frame pool via [`FrameAllocator`]; the *policy* of where that pool lives
//! stays in the kernel. Those frames are written here through the kernel's
//! linear map ([`mmu::phys_to_virt`]) — a fresh frame is not mapped in any user
//! space yet, and the kernel is the only one that can reach it at all.
//!
//! # The tables are the only record
//!
//! Tables below the root are created **lazily**, on the first mapping that needs
//! one, and a space owns a frame exactly when the descriptor mapping it carries
//! [`mmu::SW_OWNED`]. There is no list of frames beside the tables, and therefore
//! no cap on how many pages a process may have, no list to keep in step with the
//! mappings it describes, and no special case at teardown for the one kind of
//! page a space does *not* own (a device's registers). [`AddressSpace::destroy`]
//! walks the tree and frees what the tree says is ours.
//!
//! This is what lets the EL0 window be the sparse 48-bit space it should be
//! rather than the single 2 MiB leaf table it started as.

use core::arch::asm;

use staros_mm::{FrameAllocator, PhysAddr};

use crate::cache;
use crate::mmu::{
    self, user_data_page, user_device_page, user_dma_page, user_ro_page, user_text_page, ADDR_MASK,
    DESC_KIND, DESC_TABLE, DESC_VALID, PAGE_4K, SW_OWNED,
};

/// Base of the per-process EL0 window, and where every program is linked. Well
/// clear of the bottom of the address space, so that a null-ish pointer in a user
/// program faults instead of finding a page.
pub const USER_BASE: u64 = 0x8000_0000;
/// Exclusive upper bound for ELF segment virtual addresses: a loaded image must
/// lie in `[USER_BASE, USER_IMAGE_END)` — a gigabyte of room — keeping it clear
/// of the regions below, which the kernel places rather than the program.
pub const USER_IMAGE_END: u64 = 0xC000_0000;

/// Base of the anonymous-memory region: pages the task requests at runtime via
/// `MapAnon` are placed from here upward.
pub const USER_HEAP_BASE: u64 = 0x1_0000_0000;
/// Exclusive upper bound of the anonymous-memory region — four gigabytes of heap,
/// which in practice means the frame pool runs out first.
pub const USER_HEAP_END: u64 = 0x2_0000_0000;

/// User data page (read/write at EL0). The kernel seeds a per-process byte here
/// before launch; each process reads its own value, proving the spaces differ.
pub const USER_DATA_VA: u64 = 0x4_0000_0000;
/// Virtual address at which [`AddressSpace::map_device`] places a mapped MMIO
/// page for a user-space driver.
pub const USER_DEV_VA: u64 = USER_DATA_VA + PAGE_4K;

/// Where a shared-memory buffer is mapped (read/write) in each holder's space.
/// Both sharers see the same physical page here, at the same VA in their own
/// spaces. Clear of the data/device pages, the DTB and the stack.
pub const USER_SHARED_VA: u64 = 0x5_0000_0000;

/// Where a DMA buffer is mapped (non-cacheable read/write) in a driver's space.
/// Its own gigabyte, clear of every other window.
pub const USER_DMA_VA: u64 = 0x7_0000_0000;

/// Where the device tree blob is mapped, read-only, for a privileged device
/// manager to parse in user space. Clear of the data page, the device page and
/// the stack; a blob is at most a megabyte, so one gigabyte of span is ample.
pub const USER_DTB_VA: u64 = 0x6_0000_0000;

/// Where the initramfs (CPIO) archive is mapped, read-only, for the bootstrap
/// process to unpack in user space. Placed above the stack, in its own gigabyte
/// of otherwise-empty 48-bit space; our archives are kilobytes, but the window is
/// generous. Read-only at EL0 *and* EL1 so a buggy unpacker cannot corrupt the
/// image the bootloader left.
pub const USER_INITRD_VA: u64 = 0x9_0000_0000;

/// Where a display server sees the framebuffer's pixels.
///
/// Its own region because the buffer is unlike everything else a process is
/// handed: megabytes rather than a page, writable, and belonging to neither the
/// process (teardown must not free it — it is firmware's or the GPU's) nor to any
/// shared-memory object.
pub const USER_FB_VA: u64 = 0xA_0000_0000;

/// Top of the user stack (grows down); [`USER_STACK_PAGES`] sit just below it.
pub const USER_STACK_TOP: u64 = 0x8_0000_0000;

/// How many stack pages a fresh space is given up front. One is enough to enter
/// EL0 and take the first fault; the rest arrive on demand.
///
/// Mapping the whole stack eagerly costs every process its maximum stack whether
/// or not it uses it — the arithmetic that matters on a phone with dozens of
/// processes, not on a demo with seven.
pub const USER_STACK_PAGES: u64 = 1;

/// How far the stack may grow downward, in pages. Beyond this a fault is a fault:
/// the region below [`USER_STACK_LIMIT`] is a guard the kernel never fills, so
/// runaway recursion dies instead of quietly eating the frame pool.
pub const USER_STACK_MAX_PAGES: u64 = 64; // 256 KiB

/// The lowest address the stack may ever reach. Below it lies the guard region.
pub const USER_STACK_LIMIT: u64 = USER_STACK_TOP - USER_STACK_MAX_PAGES * PAGE_4K;

/// ELF `p_flags` bit: segment is executable.
const PF_X: u32 = 1;
/// ELF `p_flags` bit: segment is writable.
const PF_W: u32 = 2;

/// Descriptors per translation table.
const ENTRIES: usize = 512;
/// The level whose descriptors map 4 KiB pages rather than further tables.
const LEAF_LEVEL: u8 = 3;

/// Index into the level-`level` table for `va`. Level 0 uses bits `[47:39]` and
/// each level down shifts nine bits further right, which is all a 4 KiB-granule
/// walk is.
const fn table_index(va: u64, level: u8) -> usize {
    ((va >> (39 - 9 * level as u32)) & 0x1ff) as usize
}

/// A user process address space: the physical root table, the data frame the
/// kernel seeds before launch, the program entry point taken from the ELF, and
/// the heap cursor. Everything else it owns is recorded in the tables themselves.
///
/// This is a small `Copy` handle (four physical pointers), not an owner with a
/// `Drop` — copying it duplicates the handle, and both copies name the same
/// tables. Reclamation is therefore explicit: the kernel calls
/// [`destroy`](AddressSpace::destroy) exactly once, when the task that owns the
/// space exits, to return its frames to the allocator.
#[derive(Clone, Copy)]
pub struct AddressSpace {
    /// Physical address of the level-0 table — the value loaded into `TTBR0_EL1`,
    /// and the root every walk in this file starts from.
    root: u64,
    /// Physical frame backing [`USER_DATA_VA`], writable by the kernel through
    /// the linear map to seed per-process data.
    data_phys: u64,
    /// EL0 virtual address to enter at, taken from the loaded ELF's `e_entry`.
    entry: u64,
    /// Next free virtual address in the anonymous-memory region, bumped by
    /// [`map_anon`](AddressSpace::map_anon) as the task grows its heap.
    heap_next: u64,
}

impl AddressSpace {
    /// Build a fresh, *codeless* address space: a root table, the per-process
    /// data page and the stack — and not one kernel mapping. The program's own
    /// code and data are added afterwards by the ELF loader via
    /// [`map_segment`](AddressSpace::map_segment); the entry point is recorded
    /// with [`set_entry`](AddressSpace::set_entry).
    ///
    /// Returns `None` if the pool is exhausted, having first given back every
    /// frame it had already taken — a half-built space is torn down by the same
    /// tree walk as a finished one, so the failure path needs no bookkeeping of
    /// its own.
    ///
    /// # Safety
    /// [`mmu::init`] must have mapped the RAM frame pool as Normal memory (so a
    /// freshly allocated frame is writable at its linear-map address), and every
    /// frame `alloc` returns must be uniquely owned by this call.
    pub unsafe fn new<A: FrameAllocator>(alloc: &mut A) -> Option<Self> {
        // SAFETY: the caller guarantees the pool is mapped writable, so each
        // frame can be zeroed and filled through the linear map.
        let root = unsafe { alloc_table(alloc)? };
        let mut space = Self {
            root,
            data_phys: 0,
            entry: 0,
            heap_next: USER_HEAP_BASE,
        };
        // SAFETY: as above; `space` is a well-formed (if empty) tree from the
        // moment its root exists, which is what makes the cleanup below sound.
        match unsafe { space.populate(alloc) } {
            Some(()) => {
                // SAFETY: publish the tables before any walker (this TTBR0 is
                // loaded later).
                unsafe { asm!("dsb ish", options(nostack, preserves_flags)) };
                Some(space)
            }
            None => {
                // SAFETY: nothing has ever loaded this TTBR0, so no walk can be
                // relying on the frames; free whatever got mapped before the pool
                // ran dry.
                unsafe { free_table(alloc, root, 0) };
                None
            }
        }
    }

    /// Map the pages every space gets regardless of the program it will run: the
    /// data page and the stack.
    ///
    /// # Safety
    /// Same preconditions as [`new`](AddressSpace::new).
    unsafe fn populate<A: FrameAllocator>(&mut self, alloc: &mut A) -> Option<()> {
        // SAFETY: fresh, uniquely-owned frames, zeroed through the linear map and
        // mapped into a tree nothing is walking yet.
        unsafe {
            let data = self.map_fresh(alloc, USER_DATA_VA)?;
            self.data_phys = data;
            for i in 1..=USER_STACK_PAGES {
                self.map_fresh(alloc, USER_STACK_TOP - i * PAGE_4K)?;
            }
        }
        Some(())
    }

    /// Allocate one zeroed frame, map it read/write at `va` as a page this space
    /// owns, and return the frame's physical address.
    ///
    /// # Safety
    /// Same preconditions as [`new`](AddressSpace::new).
    unsafe fn map_fresh<A: FrameAllocator>(&self, alloc: &mut A, va: u64) -> Option<u64> {
        let frame = alloc.allocate()?.0 as u64;
        // SAFETY: fresh, uniquely-owned frame reachable through the linear map.
        unsafe { zero_frame(frame) };
        // SAFETY: `va` is one of this module's own fixed addresses or a heap
        // address checked by the caller.
        if !unsafe { self.map_page(alloc, va, user_data_page(frame) | SW_OWNED) } {
            // The table walk ran out of frames; this one is not mapped anywhere,
            // so it is ours to hand straight back.
            alloc.free(PhysAddr(frame as usize));
            return None;
        }
        Some(frame)
    }

    /// Grow the stack to cover a faulting address, if that address is a legitimate
    /// stack access. Returns `true` when a page was mapped and the faulting
    /// instruction should be retried.
    ///
    /// This is what turns "the stack is `USER_STACK_PAGES` and a fault below it is
    /// fatal" into a stack that costs what it uses. Three cases are deliberately
    /// *not* growth, and each returns `false` so the task still dies:
    ///
    /// - the address is outside `[USER_STACK_LIMIT, USER_STACK_TOP)` — some other
    ///   region faulted, or the stack ran past its limit into the guard;
    /// - the page is already mapped — then the fault was about *permissions*, not
    ///   absence, and mapping another page would paper over a real bug;
    /// - the frame pool is exhausted — memory pressure must not look like a
    ///   successful growth.
    ///
    /// # Safety
    /// Same preconditions as [`new`](AddressSpace::new). Runs against the *active*
    /// space, from the fault handler of the very task that faulted: it only adds a
    /// previously-absent page and then invalidates that one VA.
    pub unsafe fn grow_stack<A: FrameAllocator>(&self, alloc: &mut A, far: u64) -> bool {
        let va = far & !(PAGE_4K - 1);
        if !(USER_STACK_LIMIT..USER_STACK_TOP).contains(&va) {
            return false;
        }
        // SAFETY: forwarded — this space is live (we are running in it).
        if unsafe { self.lookup(va) }.is_some() {
            return false; // present already: a permission fault, not a growth request
        }
        // SAFETY: forwarded from this function's contract.
        if unsafe { self.map_fresh(alloc, va) }.is_none() {
            return false;
        }
        // Publish the descriptor and invalidate just this VA, as `map_anon` does —
        // the faulting instruction is about to be retried against it.
        // SAFETY: `va >> 12` is the page number operand `TLBI VAAE1IS` expects.
        unsafe {
            asm!(
                "dsb ish",
                "tlbi vaae1is, {v}",
                "dsb ish",
                "isb",
                v = in(reg) va >> 12,
                options(nostack, preserves_flags),
            );
        }
        true
    }

    /// Map `pages` fresh, zero-filled, read/write pages at the next free
    /// anonymous-heap virtual addresses and return the address of the first.
    /// Backs the `MapAnon` syscall: a task grows its own memory at runtime.
    ///
    /// The pages are **contiguous in virtual address space and nothing more** —
    /// each is an independently allocated frame. That is the difference from a DMA
    /// buffer, which must be physically contiguous because a device sees physical
    /// addresses; a heap is read by a CPU behind an MMU and does not care. Asking
    /// for physical contiguity here would mean a power-of-two rounding and a
    /// failure whenever memory is merely fragmented, for no benefit at all.
    ///
    /// Returns `None` if `pages` is zero, if the run would leave
    /// [`USER_HEAP_END`], or if the frame pool runs out partway. In that last case
    /// the pages already mapped **stay mapped** and the heap cursor keeps them:
    /// they belong to this space and are returned when the task is torn down.
    /// Un-mapping them would be tidier and is not free — it means walking back
    /// through the tables to hand frames back on the path where memory is already
    /// exhausted. The caller gets an error and simply does not learn an address;
    /// nothing leaks past the task's lifetime.
    ///
    /// # Safety
    /// Same preconditions as [`new`](AddressSpace::new). May run against the
    /// *active* space (it only adds previously-absent pages, then flushes those
    /// VAs from the TLB), so a task can call it on itself.
    pub unsafe fn map_anon<A: FrameAllocator>(&mut self, alloc: &mut A, pages: u64) -> Option<u64> {
        if pages == 0 {
            return None;
        }
        let first = self.heap_next;
        // Check the whole run up front, in arithmetic that cannot wrap: a request
        // large enough to overflow the addition would otherwise "fit" and start
        // mapping at a wrapped address.
        let bytes = pages.checked_mul(PAGE_4K)?;
        if first.checked_add(bytes)? > USER_HEAP_END {
            return None;
        }
        for i in 0..pages {
            let va = first + i * PAGE_4K;
            // SAFETY: forwarded from this function's contract.
            if unsafe { self.map_fresh(alloc, va) }.is_none() {
                // Out of frames partway. Keep what is mapped (see above) and
                // report failure rather than a half-length run the caller would
                // read as whole.
                self.heap_next = va;
                return None;
            }
            // Publish the new descriptor. Invalidate only *this* VA, not the whole
            // TLB: a full `tlbi vmalle1is` makes every core drop its entire TLB per
            // page, which serialised the machine when one task mapped thousands.
            // `vaae1is` (this VA, all ASIDs, inner-shareable) is the surgical form.
            // SAFETY: `va >> 12` is the page number operand `TLBI VAAE1IS` expects.
            unsafe {
                asm!(
                    "dsb ish",
                    "tlbi vaae1is, {v}",
                    "dsb ish",
                    "isb",
                    v = in(reg) va >> 12,
                    options(nostack, preserves_flags),
                );
            }
        }
        self.heap_next = first + bytes;
        Some(first)
    }

    /// Load one ELF `PT_LOAD` segment into this space: back `[vaddr, vaddr +
    /// memsz)` with freshly allocated frames, copy the segment's `file` bytes
    /// (`p_filesz`) into them, leave the remainder zero (that is the `.bss` tail),
    /// and map every page with the rights `p_flags` requests — read+execute for
    /// `PF_X`, read/write for `PF_W`, read-only otherwise (W^X). Executable pages
    /// are made coherent for instruction fetch before they can run.
    ///
    /// Returns `false` (mapping nothing further) if the segment leaves the image
    /// region or the frame pool is exhausted.
    ///
    /// # Safety
    /// Same preconditions as [`new`](AddressSpace::new): the RAM pool is covered
    /// by the kernel's linear map and writable, and every allocated frame is
    /// uniquely owned. `file` must be the segment's file image (at most `memsz`
    /// bytes).
    pub unsafe fn map_segment<A: FrameAllocator>(
        &mut self,
        alloc: &mut A,
        vaddr: u64,
        file: &[u8],
        memsz: usize,
        flags: u32,
    ) -> bool {
        if !vaddr.is_multiple_of(PAGE_4K) {
            return false;
        }
        let pages = (memsz as u64).div_ceil(PAGE_4K);
        for i in 0..pages {
            let va = vaddr + i * PAGE_4K;
            // Every segment page must land in the image region, clear of the
            // regions the kernel places. A program that would reach out of it is
            // rejected here instead of quietly clobbering them.
            if !(USER_BASE..USER_IMAGE_END).contains(&va) {
                return false;
            }
            let Some(frame) = alloc.allocate() else {
                return false;
            };
            let frame = frame.0 as u64;
            // SAFETY: fresh, uniquely-owned frame, reachable through the linear
            // map. Zero it, then overlay whatever slice of the file image falls in
            // this page; bytes past `file` stay zero, giving the `.bss` tail for
            // free.
            unsafe {
                zero_frame(frame);
                let start = (i * PAGE_4K) as usize;
                if start < file.len() {
                    let n = core::cmp::min(PAGE_4K as usize, file.len() - start);
                    core::ptr::copy_nonoverlapping(
                        file.as_ptr().add(start),
                        mmu::phys_to_virt(frame) as *mut u8,
                        n,
                    );
                }
            }
            let desc = if flags & PF_X != 0 {
                user_text_page(frame)
            } else if flags & PF_W != 0 {
                user_data_page(frame)
            } else {
                user_ro_page(frame)
            };
            // SAFETY: `va` is inside the image region, so the walk may create
            // whatever tables it needs. Executable frames were written via the
            // data path, so sync I-cache before they are ever fetched. The
            // maintenance is done at the frame's linear-map address — the only one
            // that resolves here — which is the right one regardless: `dc cvau` and
            // `ic ivau` act on the *physical* line the address translates to, so
            // any mapping of the frame will do.
            unsafe {
                if !self.map_page(alloc, va, desc | SW_OWNED) {
                    alloc.free(PhysAddr(frame as usize));
                    return false;
                }
                if flags & PF_X != 0 {
                    cache::sync_instruction(mmu::phys_to_virt(frame), PAGE_4K as usize);
                }
            }
        }
        // SAFETY: publish the new descriptors before this TTBR0 is ever loaded.
        unsafe { asm!("dsb ish", options(nostack, preserves_flags)) };
        true
    }

    /// Record the program entry point (the ELF `e_entry`) the task will drop to
    /// EL0 at.
    pub fn set_entry(&mut self, entry: u64) {
        self.entry = entry;
    }

    /// The EL0 virtual address to enter this program at.
    #[must_use]
    pub fn entry(&self) -> u64 {
        self.entry
    }

    /// Walk down to the level-3 table covering `va`, creating any table the way
    /// there is missing, and return its physical address. This is what makes the
    /// EL0 window sparse: a space pays a frame per table only for the regions it
    /// actually uses, so a program can be mapped at `USER_BASE` and its heap four
    /// gigabytes higher without anything in between costing a byte.
    ///
    /// Returns `None` if the pool cannot supply a table. Any table it did create
    /// first is already linked into the tree, so it is not leaked — teardown will
    /// find it.
    ///
    /// # Safety
    /// Same preconditions as [`new`](AddressSpace::new).
    unsafe fn leaf_table<A: FrameAllocator>(&self, alloc: &mut A, va: u64) -> Option<u64> {
        let mut table = self.root;
        for level in 0..LEAF_LEVEL {
            let index = table_index(va, level);
            // SAFETY: `table` is a table frame of this space (the root, or one a
            // previous iteration validated), and `index < 512`.
            let desc = unsafe { read_entry(table, index) };
            table = if desc & DESC_VALID == 0 {
                // SAFETY: a fresh zeroed frame becomes the next level down.
                let next = unsafe { alloc_table(alloc)? };
                // SAFETY: as above.
                unsafe { write_entry(table, index, next | DESC_TABLE) };
                next
            } else {
                debug_assert!(
                    desc & DESC_KIND == DESC_TABLE,
                    "user spaces map no blocks above level 3",
                );
                desc & ADDR_MASK
            };
        }
        Some(table)
    }

    /// Install `desc` as the mapping of `va`, creating tables as needed. Returns
    /// `false` only if the pool could not supply one.
    ///
    /// # Safety
    /// Same preconditions as [`new`](AddressSpace::new). Does not flush the TLB:
    /// callers that may be touching a live space do that themselves, once, after
    /// the last mapping.
    unsafe fn map_page<A: FrameAllocator>(&self, alloc: &mut A, va: u64, desc: u64) -> bool {
        // SAFETY: forwarded from this function's contract.
        let Some(leaf) = (unsafe { self.leaf_table(alloc, va) }) else {
            return false;
        };
        // SAFETY: `leaf` is a level-3 table of this space and the index is in range.
        unsafe { write_entry(leaf, table_index(va, LEAF_LEVEL), desc) };
        true
    }

    /// The leaf descriptor mapping `va` in this space, or `None` if nothing does.
    ///
    /// This is the walk the *hardware* does, in software: it is how the kernel
    /// answers "may EL0 touch this address?" about a space that is now a sparse
    /// 48-bit tree rather than one always-present window.
    ///
    /// # Safety
    /// The space must not have been [destroyed](AddressSpace::destroy).
    unsafe fn lookup(&self, va: u64) -> Option<u64> {
        let mut table = self.root;
        for level in 0..LEAF_LEVEL {
            // SAFETY: `table` is a table frame of this space; index is in range.
            let desc = unsafe { read_entry(table, table_index(va, level)) };
            if desc & DESC_KIND != DESC_TABLE {
                // Invalid, or a block — either way there is no level-3 table here.
                return None;
            }
            table = desc & ADDR_MASK;
        }
        // SAFETY: as above.
        let desc = unsafe { read_entry(table, table_index(va, LEAF_LEVEL)) };
        (desc & DESC_VALID != 0).then_some(desc)
    }

    /// Whether EL0 in this space may read — and, if `write`, write — every byte of
    /// `[va, va + len)`.
    ///
    /// The kernel asks this before dereferencing a pointer a user handed it. A
    /// range check cannot answer it: the EL0 window is sparse, so "inside the
    /// window" says nothing about whether a page is *there*, and following the
    /// task's own tables is the only honest answer.
    ///
    /// # Safety
    /// The space must not have been [destroyed](AddressSpace::destroy).
    #[must_use]
    pub unsafe fn user_range_ok(&self, va: u64, len: usize, write: bool) -> bool {
        let Some(end) = va.checked_add(len as u64) else {
            return false;
        };
        if len == 0 {
            return true;
        }
        let mut page = va & !(PAGE_4K - 1);
        while page < end {
            // SAFETY: forwarded from this function's contract.
            let Some(desc) = (unsafe { self.lookup(page) }) else {
                return false;
            };
            if !mmu::user_readable(desc) || (write && !mmu::user_writable(desc)) {
                return false;
            }
            page += PAGE_4K;
        }
        true
    }

    /// Tear the address space down: return every frame it privately owns — its
    /// page tables, data and stack frames, the ELF segment pages the loader mapped
    /// and every page the task grew at runtime — to `alloc`. After this the handle
    /// must not be used again.
    ///
    /// The tree is the record, so this frees exactly what the space actually has:
    /// every table, and every leaf carrying [`mmu::SW_OWNED`]. A device page
    /// mapped by [`map_device`](AddressSpace::map_device) does not carry it and so
    /// is not handed to the frame allocator — which would be a serious bug, MMIO
    /// not being RAM.
    ///
    /// # Safety
    /// This space's `TTBR0` must no longer be needed as a *source of new*
    /// translations by the time a freed frame is handed out again: freeing only
    /// marks the frames reusable (it does not erase them), so an active `TTBR0`
    /// keeps working until the caller switches away, but the frames must not be
    /// reallocated while any walk still relies on them. Every frame must have come
    /// from this same allocator via [`AddressSpace::new`]/[`map_segment`].
    pub unsafe fn destroy<A: FrameAllocator>(&self, alloc: &mut A) {
        // SAFETY: forwarded from this function's contract; `root` is level 0.
        unsafe { free_table(alloc, self.root, 0) };
    }

    /// The `TTBR0_EL1` value that activates this address space.
    #[must_use]
    pub fn ttbr0(&self) -> u64 {
        self.root
    }

    /// Seed the first byte of the user data page (read by the program at
    /// [`USER_DATA_VA`]) to distinguish this process from the others.
    ///
    /// # Safety
    /// Requires the data frame to be writable through the kernel's linear map,
    /// i.e. the same precondition as [`AddressSpace::new`], and to be seeded
    /// before this TTBR0 runs.
    pub unsafe fn write_id(&self, id: u8) {
        // SAFETY: `data_phys` is a frame this space owns, mapped as Normal RAM in
        // the linear map, so a single byte write at its linear-map address is
        // sound.
        unsafe {
            (mmu::phys_to_virt(self.data_phys) as *mut u8).write_volatile(id);
        }
    }

    /// Seed the device tree's user-space address (at [`USER_DATA_VA`] `+ 8`) and
    /// length (at `+ 16`) into the user data page, so a privileged device manager
    /// can parse the tree itself with [`map_dtb`](AddressSpace::map_dtb).
    ///
    /// This replaces the earlier per-device seed: the manager no longer receives a
    /// pre-chosen address and interrupt, it receives the *whole tree* and finds
    /// them — the same `fdt` crate the kernel uses, now on the user side.
    ///
    /// # Safety
    /// Same precondition as [`AddressSpace::write_id`]: the data frame must be
    /// writable through the linear map and seeded before this TTBR0 runs. The
    /// data page is 4 KiB, so these offsets are well within it.
    pub unsafe fn write_dtb_info(&self, dtb_va: u64, len: u32) {
        // SAFETY: `data_phys` backs a 4 KiB Normal-RAM frame this space owns;
        // offsets 8 and 16 are 8-byte aligned and inside the page.
        unsafe {
            let base = mmu::phys_to_virt(self.data_phys) as *mut u8;
            base.add(8).cast::<u64>().write_volatile(dtb_va);
            base.add(16).cast::<u32>().write_volatile(len);
        }
    }

    /// Map the device tree blob at physical `dtb_phys` (`len` bytes) into this
    /// space read-only, EL0-readable, at [`USER_DTB_VA`], and return the user
    /// virtual address of its first byte.
    ///
    /// The descriptors deliberately carry **no** [`SW_OWNED`]: the blob is
    /// firmware's memory the kernel stands on, not a frame this space may hand
    /// back at teardown — the same reasoning as [`map_device`](AddressSpace::map_device).
    /// The mapping is read-only at EL0 *and* EL1, so a buggy manager cannot
    /// corrupt the tree the kernel itself parsed.
    ///
    /// # Safety
    /// Must run at EL1 with this space's tables writable through the linear map,
    /// before this TTBR0 is active. `[dtb_phys, dtb_phys + len)` must be the real
    /// blob.
    pub unsafe fn map_dtb<A: FrameAllocator>(
        &self,
        alloc: &mut A,
        dtb_phys: u64,
        len: usize,
    ) -> Option<u64> {
        let page_off = dtb_phys & (PAGE_4K - 1);
        let first_page = dtb_phys - page_off;
        let pages = (page_off + len as u64).div_ceil(PAGE_4K);
        for i in 0..pages {
            let pa = first_page + i * PAGE_4K;
            // SAFETY: forwarded from this function's contract; a fresh table frame
            // may be allocated for the walk, and the page is Normal RO EL0.
            if !unsafe { self.map_page(alloc, USER_DTB_VA + i * PAGE_4K, user_ro_page(pa)) } {
                return None;
            }
        }
        Some(USER_DTB_VA + page_off)
    }

    /// Seed the initramfs's user-space address (at [`USER_DATA_VA`] `+ 24`) and
    /// length (at `+ 32`) into the user data page, so the bootstrap process can
    /// unpack the CPIO archive itself. A length of zero means "no initramfs" — the
    /// bootloader left none — and the process must handle that.
    ///
    /// # Safety
    /// Same precondition as [`AddressSpace::write_id`]: the data frame must be
    /// writable through the linear map and seeded before this TTBR0 runs. The data
    /// page is 4 KiB, so these offsets are well within it (and clear of the DTB
    /// info at +8/+16).
    pub unsafe fn write_initrd_info(&self, initrd_va: u64, len: u32) {
        // SAFETY: `data_phys` backs a 4 KiB Normal-RAM frame this space owns;
        // offsets 24 and 32 are 8-byte aligned and inside the page.
        unsafe {
            let base = mmu::phys_to_virt(self.data_phys) as *mut u8;
            base.add(24).cast::<u64>().write_volatile(initrd_va);
            base.add(32).cast::<u32>().write_volatile(len);
        }
    }

    /// Map the initramfs archive at physical `initrd_phys` (`len` bytes) into this
    /// space read-only, EL0-readable, at [`USER_INITRD_VA`], and return the user
    /// virtual address of its first byte. Same read-only, un-owned (no [`SW_OWNED`])
    /// treatment as [`map_dtb`](AddressSpace::map_dtb): the archive is the
    /// bootloader's memory, excluded from the frame pool, not a reclaimable frame.
    ///
    /// # Safety
    /// Must run at EL1 with this space's tables writable through the linear map,
    /// before this TTBR0 is active. `[initrd_phys, initrd_phys + len)` must be the
    /// real archive the bootloader loaded.
    pub unsafe fn map_initrd<A: FrameAllocator>(
        &self,
        alloc: &mut A,
        initrd_phys: u64,
        len: usize,
    ) -> Option<u64> {
        let page_off = initrd_phys & (PAGE_4K - 1);
        let first_page = initrd_phys - page_off;
        let pages = (page_off + len as u64).div_ceil(PAGE_4K);
        for i in 0..pages {
            let pa = first_page + i * PAGE_4K;
            // SAFETY: forwarded from this function's contract; a fresh table frame
            // may be allocated for the walk, and the page is Normal RO EL0.
            if !unsafe { self.map_page(alloc, USER_INITRD_VA + i * PAGE_4K, user_ro_page(pa)) } {
                return None;
            }
        }
        Some(USER_INITRD_VA + page_off)
    }

    /// Map the framebuffer's pixels (`len` bytes at physical `phys`) into this
    /// space at [`USER_FB_VA`], read/write and EL0-accessible, and return that
    /// virtual address.
    ///
    /// Carries no [`SW_OWNED`], like a device or shared mapping and for the
    /// strongest version of the same reason: these frames were never the frame
    /// allocator's. They are whatever the firmware or the GPU set aside before the
    /// kernel had a pool, deliberately excluded from it — handing them back at
    /// teardown would put memory the kernel does not own into circulation.
    ///
    /// Mapped as ordinary Normal memory rather than Device: a framebuffer is
    /// written like memory, in bulk, and forcing every store through a Device
    /// mapping would cost a display server most of its bandwidth for nothing. What
    /// it *does* cost is a cache-maintenance obligation on hardware whose scanout
    /// is not coherent with the CPU — real on a Pi, absent under QEMU, and the
    /// server's problem rather than this function's.
    ///
    /// # Safety
    /// As [`new`](AddressSpace::new), plus: `[phys, phys + len)` must be the real
    /// pixel buffer, outside the frame pool, and this space must be the only one
    /// given it.
    pub unsafe fn map_framebuffer<A: FrameAllocator>(
        &self,
        alloc: &mut A,
        phys: u64,
        len: usize,
    ) -> Option<u64> {
        let page_off = phys & (PAGE_4K - 1);
        let first_page = phys - page_off;
        let pages = (page_off + len as u64).div_ceil(PAGE_4K);
        for i in 0..pages {
            let pa = first_page + i * PAGE_4K;
            // SAFETY: forwarded from this function's contract; a table frame may be
            // allocated for the walk, and the page is Normal RW EL0.
            if !unsafe { self.map_page(alloc, USER_FB_VA + i * PAGE_4K, user_data_page(pa)) } {
                return None;
            }
        }
        Some(USER_FB_VA + page_off)
    }

    /// Record where the framebuffer landed and what shape it is, in this space's
    /// data page: virtual address at `+40`, width at `+48`, height at `+52`,
    /// stride at `+56`, all as the display server reads them.
    ///
    /// Seeded rather than asked for over a syscall because the server cannot ask
    /// before it runs, and what it needs is four numbers the kernel already knows.
    ///
    /// # Safety
    /// The space must have been built by [`new`](AddressSpace::new) (so its data
    /// frame exists) and not yet be running.
    pub unsafe fn write_fb_info(&self, fb_va: u64, width: u32, height: u32, stride: u32) {
        // SAFETY: `data_phys` backs a 4 KiB Normal-RAM frame this space owns;
        // offsets 40..60 are aligned and inside the page.
        unsafe {
            let base = mmu::phys_to_virt(self.data_phys) as *mut u8;
            base.add(40).cast::<u64>().write_volatile(fb_va);
            base.add(48).cast::<u32>().write_volatile(width);
            base.add(52).cast::<u32>().write_volatile(height);
            base.add(56).cast::<u32>().write_volatile(stride);
        }
    }

    /// Map a shared-memory buffer (`pages` contiguous frames at physical `phys`)
    /// into this space at [`USER_SHARED_VA`], read/write and EL0-accessible, and
    /// return that virtual address.
    ///
    /// Like [`map_device`](AddressSpace::map_device), the descriptors carry **no**
    /// [`SW_OWNED`]: the frames belong to the shared-memory *object*, not to this
    /// space, so teardown must not hand them back — `obj::free_shared` reclaims
    /// them once every task has exited. Two tasks mapping the same object thus see
    /// the same physical page, and neither one's teardown frees it out from under
    /// the other.
    ///
    /// # Safety
    /// Must run at EL1 with this space's tables writable through the linear map.
    /// `[phys, phys + pages*4KiB)` must be frames the kernel allocated for this
    /// shared object.
    pub unsafe fn map_shared<A: FrameAllocator>(
        &self,
        alloc: &mut A,
        phys: u64,
        pages: u32,
    ) -> Option<u64> {
        for i in 0..u64::from(pages) {
            let va = USER_SHARED_VA + i * PAGE_4K;
            // SAFETY: forwarded from this function's contract; a table frame may be
            // allocated for the walk, and the page is Normal RW EL0.
            if !unsafe { self.map_page(alloc, va, user_data_page(phys + i * PAGE_4K)) } {
                return None;
            }
        }
        // SAFETY: publish the new mappings to the walker for the active regime.
        unsafe {
            asm!(
                "dsb ish",
                "tlbi vmalle1is",
                "dsb ish",
                "isb",
                options(nostack, preserves_flags),
            );
        }
        Some(USER_SHARED_VA)
    }

    /// Map a DMA buffer (`pages` physically-contiguous frames at `phys`) into this
    /// space at [`USER_DMA_VA`], **non-cacheable** read/write and EL0-accessible,
    /// and return that virtual address.
    ///
    /// Non-cacheable ([`user_dma_page`]) is the coherency contract: a device
    /// writing straight to these frames and the driver reading them through this
    /// mapping see the same bytes with no cache maintenance. As with
    /// [`map_shared`](AddressSpace::map_shared) the descriptors carry no
    /// [`SW_OWNED`] — the frames belong to the DMA object, reclaimed centrally.
    ///
    /// # Safety
    /// Must run at EL1 with this space's tables writable through the linear map.
    /// `[phys, phys + pages*4KiB)` must be the contiguous frames the kernel
    /// allocated for this DMA object.
    pub unsafe fn map_dma<A: FrameAllocator>(
        &self,
        alloc: &mut A,
        phys: u64,
        pages: u32,
    ) -> Option<u64> {
        for i in 0..u64::from(pages) {
            let va = USER_DMA_VA + i * PAGE_4K;
            // SAFETY: forwarded from this function's contract; a table frame may be
            // allocated for the walk, and the page is Normal-NC RW EL0.
            if !unsafe { self.map_page(alloc, va, user_dma_page(phys + i * PAGE_4K)) } {
                return None;
            }
        }
        // SAFETY: publish the new mappings to the walker for the active regime.
        unsafe {
            asm!(
                "dsb ish",
                "tlbi vmalle1is",
                "dsb ish",
                "isb",
                options(nostack, preserves_flags),
            );
        }
        Some(USER_DMA_VA)
    }

    /// Map the MMIO page at physical `dev_phys` into this space at [`USER_DEV_VA`]
    /// as EL0-accessible Device memory, and return that virtual address. This is
    /// the mechanism behind the `MapMemory` syscall: it hands a user-space driver
    /// direct, unprivileged access to a device's registers.
    ///
    /// This only adds a previously-absent page, so it is safe to do to the
    /// *active* address space. Returns `None` if the pool cannot supply a table
    /// for the walk.
    ///
    /// The descriptor deliberately does **not** carry [`mmu::SW_OWNED`]: a
    /// device's registers are not a frame this space may hand back at teardown.
    ///
    /// # Safety
    /// Must run at EL1 with this space's tables writable through the linear map.
    /// `dev_phys` must be a real device page the caller is permitted to map (the
    /// kernel gates which addresses reach here). Rewrites a live page table.
    pub unsafe fn map_device<A: FrameAllocator>(&self, alloc: &mut A, dev_phys: u64) -> Option<u64> {
        // A page maps a page, but a device does not have to start on one. Round
        // down to map, and hand back the address *of the device* — the offset
        // within the page, added back.
        //
        // This was wrong until the first unaligned device turned up. A PL011 sits
        // at 0x9000000 and a GIC at 0x8000000, so returning the page address
        // happened to be returning the device address, and the bug had nothing to
        // stand out against. QEMU's virtio-mmio slots are 0x200 bytes apart:
        // slot 31 is at 0xa003e00, and a driver handed 0xa003000 reads slot 24 —
        // which is empty, answers every register with zero, and looks exactly like
        // a machine with no such device.
        let page_off = dev_phys & (PAGE_4K - 1);
        let page = dev_phys - page_off;
        // SAFETY: forwarded from this function's contract. The barrier/TLB flush
        // publish the new mapping to the walker for the currently active regime.
        unsafe {
            if !self.map_page(alloc, USER_DEV_VA, user_device_page(page)) {
                return None;
            }
            asm!(
                "dsb ish",
                "tlbi vmalle1is",
                "dsb ish",
                "isb",
                options(nostack, preserves_flags),
            );
        }
        Some(USER_DEV_VA + page_off)
    }
}

/// Allocate one frame and zero it for use as a translation table.
///
/// # Safety
/// The returned frame must be writable through the kernel's linear map (see
/// [`AddressSpace::new`]).
unsafe fn alloc_table<A: FrameAllocator>(alloc: &mut A) -> Option<u64> {
    let frame = alloc.allocate()?.0 as u64;
    // SAFETY: fresh, uniquely-owned frame; in the linear map per the contract.
    unsafe { zero_frame(frame) };
    Some(frame)
}

/// Zero all 512 descriptor slots of a table frame, reaching it through the
/// kernel's linear map.
///
/// # Safety
/// `phys` must be a writable 4 KiB frame of Normal RAM covered by the linear map
/// (see [`AddressSpace::new`]).
unsafe fn zero_frame(phys: u64) {
    let base = mmu::phys_to_virt(phys) as *mut u64;
    for i in 0..ENTRIES {
        // SAFETY: `i < 512`, so the offset stays within the 4 KiB frame.
        unsafe { base.add(i).write_volatile(0) };
    }
}

/// Write a single descriptor into a table frame, reaching it through the
/// kernel's linear map.
///
/// # Safety
/// `table_phys` must be a writable table frame covered by the linear map and
/// `index` must be `< 512`.
unsafe fn write_entry(table_phys: u64, index: usize, desc: u64) {
    // SAFETY: caller guarantees a valid table frame and in-range index.
    unsafe {
        (mmu::phys_to_virt(table_phys) as *mut u64)
            .add(index)
            .write_volatile(desc)
    };
}

/// Read a single descriptor out of a table frame, through the kernel's linear map.
///
/// # Safety
/// `table_phys` must be a table frame covered by the linear map and `index` must
/// be `< 512`.
unsafe fn read_entry(table_phys: u64, index: usize) -> u64 {
    // SAFETY: caller guarantees a valid table frame and in-range index.
    unsafe {
        (mmu::phys_to_virt(table_phys) as *const u64)
            .add(index)
            .read_volatile()
    }
}

/// Free the sub-tree rooted at the level-`level` table `table`, then the table
/// itself: every table below it, and every leaf the space owns.
///
/// The recursion is bounded by the walk itself — four levels, so at most three
/// frames of stack, which is why this can be a plain recursive function in a
/// kernel with a fixed 64 KiB boot stack.
///
/// # Safety
/// Every frame reachable from `table` must have come from `alloc`, and nothing
/// may still be translating through this tree (see
/// [`destroy`](AddressSpace::destroy)).
unsafe fn free_table<A: FrameAllocator>(alloc: &mut A, table: u64, level: u8) {
    for index in 0..ENTRIES {
        // SAFETY: `table` is a table frame per the contract; `index < 512`.
        let desc = unsafe { read_entry(table, index) };
        if desc & DESC_VALID == 0 {
            continue;
        }
        if level < LEAF_LEVEL {
            debug_assert!(
                desc & DESC_KIND == DESC_TABLE,
                "user spaces map no blocks above level 3",
            );
            // SAFETY: a table descriptor's output address is the next table down,
            // and the depth is bounded by `LEAF_LEVEL`.
            unsafe { free_table(alloc, desc & ADDR_MASK, level + 1) };
        } else if desc & SW_OWNED != 0 {
            // A leaf this space allocated. Anything without the bit — a device's
            // registers — is somebody else's and stays untouched.
            alloc.free(PhysAddr((desc & ADDR_MASK) as usize));
        }
    }
    alloc.free(PhysAddr(table as usize));
}
