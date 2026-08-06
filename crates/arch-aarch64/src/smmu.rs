//! ARM SMMUv3 — the IOMMU that makes DMA safe in a microkernel.
//!
//! A driver in EL0 with MMIO access to a DMA-capable device is only isolated if
//! *the device itself* is confined: a bus master programmed with a physical
//! address writes there directly, and the CPU's MMU never sees the access. The
//! SMMU is the second MMU that does — it translates and checks every transaction
//! a device makes, against per-device tables the kernel owns. Without it the
//! whole capability model is decorative the moment a driver touches DMA
//! (roadmap 2.3).
//!
//! This is the *configuration* half: probe the SMMU, stand up its command and
//! event queues and a stream table, and bring it up **default-deny** — every
//! stream that is not explicitly configured aborts. That is the safe ground state
//! the capability model builds on; binding a specific device's StreamID to a
//! translation table (so a driver's DMA buffer, and nothing else, is reachable)
//! is the enforcement step that follows.
//!
//! Registers and structures follow the ARM SMMUv3 architecture specification;
//! the bring-up order mirrors Linux's `arm-smmu-v3` (probe IDRs, program the
//! tables while disabled, enable last).

use core::ptr::{read_volatile, write_volatile};

use staros_abi::error::{KError, KResult};
use staros_iommu as fmt;
use staros_mm::{FrameAllocator, PAGE_SIZE};

use crate::mmu::phys_to_virt;

// --- Register offsets from the SMMU base (SMMUv3 spec, Page 0) ---------------
const IDR0: usize = 0x000;
const IDR1: usize = 0x004;
const IDR5: usize = 0x014;
const AIDR: usize = 0x01c;
const CR0: usize = 0x020;
const CR0ACK: usize = 0x024;
const CR1: usize = 0x028;
const CR2: usize = 0x02c;
const GBPA: usize = 0x044;
const IRQ_CTRL: usize = 0x050;
const GERROR: usize = 0x060;
const GERRORN: usize = 0x064;
const STRTAB_BASE: usize = 0x080;
const STRTAB_BASE_CFG: usize = 0x088;
const CMDQ_BASE: usize = 0x090;
const CMDQ_PROD: usize = 0x098;
const CMDQ_CONS: usize = 0x09c;
const EVENTQ_BASE: usize = 0x0a0;
const EVENTQ_PROD: usize = 0x100a8;
const EVENTQ_CONS: usize = 0x100ac;

// --- IDR0 fields -------------------------------------------------------------
/// `IDR0.ST_LEVEL[28:27]`: 0 = linear stream table supported.
const IDR0_ST_LEVEL: u32 = 0b11 << 27;
/// `IDR0.S1P[1]`: stage-1 translation supported.
const IDR0_S1P: u32 = 1 << 1;
/// `IDR0.S2P[0]`: stage-2 translation supported.
const IDR0_S2P: u32 = 1 << 0;

// --- CR0 bits ----------------------------------------------------------------
const CR0_SMMUEN: u32 = 1 << 0;
const CR0_EVENTQEN: u32 = 1 << 2;
const CR0_CMDQEN: u32 = 1 << 3;

/// `GBPA.ABORT[20]` — abort transactions that miss the stream table (rather than
/// letting them bypass). `GBPA.UPDATE[31]` must be set for a write to take.
const GBPA_ABORT: u32 = 1 << 20;
const GBPA_UPDATE: u32 = 1 << 31;

/// `STRTAB_BASE_CFG.FMT` linear = 0; `LOG2SIZE` in `[5:0]`.
const STRTAB_BASE_CFG_FMT_LINEAR: u32 = 0;

/// Queue base register: `[4:0]` = LOG2SIZE, `[51:5]` = base address bits, and
/// bit 62/63 are WA/RA hints we leave clear.
const Q_BASE_ADDR_MASK: u64 = 0x000f_ffff_ffff_ffe0;

/// One StreamTableEntry is 64 bytes; one command 16; one event 32.
const STE_BYTES: usize = 64;
const CMD_BYTES: usize = 16;
const EVT_BYTES: usize = 32;
const PAGE: usize = 4096;

/// Log2 of how many stream-table entries we install. QEMU `virt` exposes a
/// 16-bit StreamID space, but a full linear table (65536 × 64 B = 4 MiB) is
/// wasteful for a bring-up whose every entry is "abort" anyway; we install a
/// modest table and rely on `GBPA.ABORT` to catch StreamIDs past its end.
const STRTAB_LOG2: u32 = 8; // 256 entries × 64 B = 16 KiB

/// A memory buffer the kernel allocated for the SMMU: its physical base and how
/// many 4 KiB pages it spans.
#[derive(Clone, Copy)]
pub struct Buf {
    /// Physical base address.
    pub phys: u64,
    /// Number of 4 KiB pages.
    pub pages: usize,
}

/// What the SMMU reported and how we configured it — for an honest boot line.
#[derive(Clone, Copy)]
pub struct SmmuConfig {
    /// Raw `AIDR` — the architecture revision *within* SMMUv3 (that it is v3 we
    /// know from the `arm,smmu-v3` compatible and this register layout).
    pub aidr: u32,
    /// StreamID width the hardware supports (`IDR1.SIDSIZE`).
    pub sid_bits: u32,
    /// Whether stage-1 translation is supported (`IDR0.S1P`).
    pub s1p: bool,
    /// Whether stage-2 translation is supported (`IDR0.S2P`).
    pub s2p: bool,
    /// Log2 of the stream-table size we installed.
    pub strtab_log2: u32,
    /// Whether `CR0.SMMUEN` read back set (the enable took effect).
    pub enabled: bool,
}

/// The three buffers the kernel must allocate for us: a stream table and the
/// command and event queues. Sizes are fixed by [`buffer_pages`].
#[derive(Clone, Copy)]
pub struct Buffers {
    /// Stream table (StreamID → StreamTableEntry).
    pub strtab: Buf,
    /// Command queue.
    pub cmdq: Buf,
    /// Event queue (faults land here).
    pub eventq: Buf,
}

/// The page counts the kernel must allocate for [`Buffers`], as
/// `(strtab, cmdq, eventq)`. Fixed so the caller can allocate before probing.
#[must_use]
pub fn buffer_pages() -> (usize, usize, usize) {
    let strtab = (STE_BYTES << STRTAB_LOG2).div_ceil(PAGE);
    let cmdq = (CMD_BYTES << CMDQ_LOG2).div_ceil(PAGE);
    let eventq = (EVT_BYTES << EVENTQ_LOG2).div_ceil(PAGE);
    (strtab, cmdq, eventq)
}

/// Log2 entries of the command and event queues.
const CMDQ_LOG2: u32 = 8; // 256 × 16 B = 4 KiB
const EVENTQ_LOG2: u32 = 8; // 256 × 32 B = 8 KiB

#[inline]
unsafe fn r32(base: usize, off: usize) -> u32 {
    // SAFETY: `base + off` is a valid SMMU register in the device linear map.
    unsafe { read_volatile((base + off) as *const u32) }
}

#[inline]
unsafe fn w32(base: usize, off: usize, val: u32) {
    // SAFETY: as `r32`, for a writable register.
    unsafe { write_volatile((base + off) as *mut u32, val) }
}

#[inline]
unsafe fn w64(base: usize, off: usize, val: u64) {
    // SAFETY: as `r32`, 64-bit.
    unsafe { write_volatile((base + off) as *mut u64, val) }
}

/// Spin until `CR0ACK` reflects `expected` (the SMMU acknowledges an enable or
/// disable), or give up. Returns whether it converged.
unsafe fn wait_cr0ack(base: usize, expected: u32) -> bool {
    for _ in 0..1_000_000 {
        // SAFETY: reading CR0ACK is side-effect free.
        if unsafe { r32(base, CR0ACK) } == expected {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// Probe and bring up the SMMU at physical `base`, using the kernel-allocated
/// `bufs`, in a default-deny (`GBPA.ABORT`) configuration.
///
/// On success returns both the [`SmmuConfig`] to report and a live [`Smmu`]
/// handle the kernel keeps to bind streams later (roadmap 2.3 enforcement).
///
/// # Safety
/// `base` must be the SMMU's MMIO base as the device tree reported it, reachable
/// through the kernel's device linear map. `bufs` must be zeroed, physically
/// contiguous allocations of at least [`buffer_pages`] each. Runs once at EL1.
pub unsafe fn init(base_phys: u64, bufs: Buffers) -> KResult<(SmmuConfig, Smmu)> {
    let base = phys_to_virt(base_phys) as usize;

    // SAFETY: reading the ID registers is side-effect free and `base` is mapped.
    let (idr0, idr1, _idr5, aidr) = unsafe {
        (r32(base, IDR0), r32(base, IDR1), r32(base, IDR5), r32(base, AIDR))
    };

    let sid_bits = idr1 & 0x3f;
    let s1p = idr0 & IDR0_S1P != 0;
    let s2p = idr0 & IDR0_S2P != 0;
    if idr0 & IDR0_ST_LEVEL == IDR0_ST_LEVEL {
        // 0b11 is reserved for ST_LEVEL — a sign we are not talking to a real SMMU.
        return Err(KError::NotSupported);
    }

    // SAFETY: the whole bring-up runs at EL1 on a mapped SMMU, disabled first.
    let enabled = unsafe {
        // 1. Disable everything and wait for the SMMU to acknowledge.
        w32(base, CR0, 0);
        if !wait_cr0ack(base, 0) {
            return Err(KError::NotSupported);
        }
        // Mask all IRQs during setup; we poll rather than take SMMU interrupts.
        w32(base, IRQ_CTRL, 0);
        // Clear any latched global errors so GERROR/GERRORN agree.
        let gerror = r32(base, GERROR);
        w32(base, GERRORN, gerror);

        // 2. Command queue: base + size, empty (prod = cons = 0).
        w64(base, CMDQ_BASE, (bufs.cmdq.phys & Q_BASE_ADDR_MASK) | u64::from(CMDQ_LOG2));
        w32(base, CMDQ_PROD, 0);
        w32(base, CMDQ_CONS, 0);

        // 3. Event queue: base + size, empty.
        w64(base, EVENTQ_BASE, (bufs.eventq.phys & Q_BASE_ADDR_MASK) | u64::from(EVENTQ_LOG2));
        w32(base, EVENTQ_PROD, 0);
        w32(base, EVENTQ_CONS, 0);

        // 4. Stream table: a linear table whose entries are all zero — an invalid
        //    StreamTableEntry, which the SMMU treats as "abort this stream". The
        //    caller zeroed the buffer, so this is default-deny with no per-entry
        //    work. `STRTAB_BASE_CFG` declares the format and log2 size.
        w64(base, STRTAB_BASE, bufs.strtab.phys & Q_BASE_ADDR_MASK);
        w32(base, STRTAB_BASE_CFG, STRTAB_BASE_CFG_FMT_LINEAR | STRTAB_LOG2);

        // 5. Miss policy: abort (not bypass) any StreamID the table does not cover.
        w32(base, GBPA, GBPA_UPDATE | GBPA_ABORT);

        // 6. Conservative CR1/CR2 (cacheable, inner-shareable table walks).
        w32(base, CR1, 0);
        w32(base, CR2, 0);

        // 7. Enable the queues, then the SMMU, acknowledging each step.
        w32(base, CR0, CR0_CMDQEN | CR0_EVENTQEN);
        let _ = wait_cr0ack(base, CR0_CMDQEN | CR0_EVENTQEN);
        w32(base, CR0, CR0_CMDQEN | CR0_EVENTQEN | CR0_SMMUEN);
        wait_cr0ack(base, CR0_CMDQEN | CR0_EVENTQEN | CR0_SMMUEN)
    };

    let config = SmmuConfig {
        aidr,
        sid_bits,
        s1p,
        s2p,
        strtab_log2: STRTAB_LOG2,
        enabled,
    };
    let handle = Smmu {
        base,
        strtab_phys: bufs.strtab.phys,
        cmdq_phys: bufs.cmdq.phys,
        cmdq_prod: 0,
    };
    Ok((config, handle))
}

/// A live SMMU: the addresses and command-queue producer index a later
/// [`bind_stream`](Smmu::bind_stream) needs. The kernel keeps exactly one, under
/// its own lock — this type has no interior mutability, so all synchronisation is
/// the caller's.
pub struct Smmu {
    /// SMMU MMIO base as a virtual address (device linear map).
    base: usize,
    /// Physical base of the linear stream table.
    strtab_phys: u64,
    /// Physical base of the command queue.
    cmdq_phys: u64,
    /// Monotonic command-queue producer counter. Its low `CMDQ_LOG2` bits are the
    /// write index and the next bit is the wrap flag, which is exactly the
    /// `CMDQ_PROD` register layout for a queue of `2^CMDQ_LOG2` entries.
    cmdq_prod: u32,
}

/// Entries in the command queue (`1 << CMDQ_LOG2`).
const CMDQ_ENTRIES: u32 = 1 << CMDQ_LOG2;
/// Mask selecting index + wrap bit — the field the `CMDQ_PROD`/`CMDQ_CONS`
/// registers hold.
const CMDQ_PROD_MASK: u32 = (CMDQ_ENTRIES << 1) - 1;

/// Stage-2 walk parameters for the identity tables we build: 4 KiB granule, four
/// levels, leaf at level 3. Matches [`fmt::Stage2Regime::stage2`].
const S2_LEAF_LEVEL: u8 = 3;
/// Next-table / output-address bits [47:12] of a stage-2 descriptor.
const S2_ADDR_MASK: u64 = 0x0000_ffff_ffff_f000;
/// The VMID all kernel-owned DMA stage-2 tables are tagged with. They never share
/// a translation, so one VMID is enough; it only labels TLB entries.
const DMA_VMID: u16 = 1;

/// A `dsb ish` barrier: make our stream-table and command-queue writes visible to
/// the SMMU before we ring the producer doorbell.
#[inline]
fn dsb_ish() {
    // SAFETY: a barrier has no operands and only orders memory.
    unsafe { core::arch::asm!("dsb ish", options(nostack, preserves_flags)) };
}

/// Index into the level-`level` stage-2 table for input address `va` (4 KiB
/// granule): the same 9-bits-per-level split a stage-1 walk uses.
const fn s2_index(va: u64, level: u8) -> usize {
    ((va >> (39 - 9 * level as u32)) & 0x1ff) as usize
}

impl Smmu {
    /// Bind `streamid` to the DMA buffer at physical `buf_phys` spanning
    /// `buf_pages` 4 KiB pages: build a stage-2 table that identity-maps **exactly**
    /// that buffer, install a stage-2-translate STE for the stream, and invalidate
    /// the SMMU's cached configuration so the next transaction re-fetches it. After
    /// this the device behind `streamid` may reach those pages and aborts on every
    /// other address it emits.
    ///
    /// After writing the STE we post `CMD_CFGI_STE` + `CMD_SYNC` and wait for
    /// `CMDQ_CONS` to catch up (and check `GERROR`), so the SMMU has demonstrably
    /// consumed our commands. The STE and table *contents* are pinned to the spec by
    /// the `staros-iommu` host tests, and — since `smmu_dma_test` — proven end to end
    /// against a real bus master (the `edu` device) that is translated to the mapped
    /// page and aborted on any other.
    ///
    /// # Safety
    /// `self` must describe a brought-up SMMU. `buf_phys`/`buf_pages` must be a
    /// contiguous run of RAM in the linear map (a DMA buffer). `alloc` supplies
    /// zeroed table frames from that same map. Runs at EL1 under the kernel's lock.
    pub unsafe fn bind_stream<A: FrameAllocator>(
        &mut self,
        streamid: u32,
        buf_phys: u64,
        buf_pages: usize,
        alloc: &mut A,
    ) -> KResult<u64> {
        // Identity: the device addresses the buffer by its physical address.
        // SAFETY: forwarded to `bind_stream_at`.
        unsafe { self.bind_stream_at(streamid, buf_phys, buf_phys, buf_pages, alloc) }
    }

    /// Bind `streamid` with an explicit input→output mapping: the device's IOVA
    /// `iova` (and the `pages-1` pages after it) translate to physical `pa`. This is
    /// the general form of [`bind_stream`] (which is just `iova == pa`), needed when
    /// a device cannot emit the buffer's physical address directly — e.g. a bus
    /// master with a narrow DMA address width must be handed a low IOVA that the
    /// SMMU relocates to high RAM.
    ///
    /// # Safety
    /// As [`bind_stream`]; additionally `iova` is page-aligned and within the
    /// stage-2 input range this SMMU is configured for.
    pub unsafe fn bind_stream_at<A: FrameAllocator>(
        &mut self,
        streamid: u32,
        iova: u64,
        buf_phys: u64,
        buf_pages: usize,
        alloc: &mut A,
    ) -> KResult<u64> {
        if buf_pages == 0 || (streamid as usize) >= (1usize << STRTAB_LOG2) {
            return Err(KError::InvalidArgument);
        }
        // SAFETY: forwarded — contiguous buffer, allocator over the linear map.
        let s2ttb = unsafe { build_stage2_map(iova, buf_phys, buf_pages, alloc) }
            .ok_or(KError::OutOfResources)?;

        let ste = fmt::stage2_ste(s2ttb, fmt::Stage2Regime::stage2(DMA_VMID));
        // SAFETY: `streamid` is within the linear table (checked) and `strtab_phys`
        // is our stream table in the linear map.
        unsafe { write_ste(self.strtab_phys, streamid, &ste) };

        // SAFETY: pushing into our own command queue and ringing the doorbell.
        unsafe {
            self.push(fmt::cmd_cfgi_ste(streamid));
            self.push(fmt::cmd_sync());
            self.doorbell();
        }
        // SAFETY: polling CONS / reading GERROR is side-effect free.
        if !unsafe { self.wait_consumed() } || unsafe { self.global_error() } {
            // The SMMU did not drain our commands, or latched an error: unwind so
            // we neither leave a half-bound stream nor leak the table. Distinct from
            // "no IOMMU" (which never reaches here) — `WouldBlock` says the SMMU is
            // present but refused/stalled on what we posted.
            // SAFETY: `s2ttb` is the tree we just built; frees only table frames.
            unsafe {
                write_ste(self.strtab_phys, streamid, &fmt::abort_ste());
                free_stage2_table(s2ttb, alloc);
            }
            return Err(KError::WouldBlock);
        }
        Ok(s2ttb)
    }

    /// Unbind `streamid`: write an abort STE back, invalidate the cached entry, and
    /// free the stage-2 table rooted at `s2ttb` (returned by [`bind_stream`]). The
    /// buffer frames themselves are not freed — they belong to the DMA object.
    ///
    /// # Safety
    /// `s2ttb` must be a table this SMMU built for `streamid` and not since freed.
    pub unsafe fn unbind_stream<A: FrameAllocator>(
        &mut self,
        streamid: u32,
        s2ttb: u64,
        alloc: &mut A,
    ) {
        if (streamid as usize) >= (1usize << STRTAB_LOG2) {
            return;
        }
        // SAFETY: in-range stream, our stream table.
        unsafe { write_ste(self.strtab_phys, streamid, &fmt::abort_ste()) };
        // SAFETY: our command queue.
        unsafe {
            self.push(fmt::cmd_cfgi_ste(streamid));
            self.push(fmt::cmd_sync());
            self.doorbell();
            let _ = self.wait_consumed();
            free_stage2_table(s2ttb, alloc);
        }
    }

    /// Push one command into the queue at the current producer slot and advance the
    /// (monotonic) producer counter. Does not ring the doorbell.
    ///
    /// # Safety
    /// The queue has room (we post at most two commands between drains, into a
    /// 256-entry queue) and `cmdq_phys` is our queue in the linear map.
    unsafe fn push(&mut self, cmd: [u64; fmt::CMD_DWORDS]) {
        let slot = (self.cmdq_prod & (CMDQ_ENTRIES - 1)) as usize;
        let base = phys_to_virt(self.cmdq_phys) as *mut u64;
        // SAFETY: `slot < CMDQ_ENTRIES`, so both doublewords stay within the queue.
        unsafe {
            write_volatile(base.add(slot * fmt::CMD_DWORDS), cmd[0]);
            write_volatile(base.add(slot * fmt::CMD_DWORDS + 1), cmd[1]);
        }
        self.cmdq_prod = self.cmdq_prod.wrapping_add(1);
    }

    /// Publish the producer index to the SMMU after a barrier, so it begins
    /// consuming the commands we wrote.
    ///
    /// # Safety
    /// `base` is the SMMU MMIO base.
    unsafe fn doorbell(&self) {
        dsb_ish();
        // SAFETY: writing the producer register of our SMMU.
        unsafe { w32(self.base, CMDQ_PROD, self.cmdq_prod & CMDQ_PROD_MASK) };
    }

    /// Spin until the consumer index reaches the producer (the SMMU has drained
    /// every command we posted), or give up. Returns whether it converged.
    ///
    /// # Safety
    /// `base` is the SMMU MMIO base.
    unsafe fn wait_consumed(&self) -> bool {
        let want = self.cmdq_prod & CMDQ_PROD_MASK;
        for _ in 0..1_000_000 {
            // SAFETY: reading CONS is side-effect free.
            if unsafe { r32(self.base, CMDQ_CONS) } & CMDQ_PROD_MASK == want {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }

    /// Has the SMMU latched a global error? `GERROR != GERRORN` means an active,
    /// unacknowledged error (e.g. `CERROR` from a malformed command) — the signal
    /// that the SMMU rejected what we posted rather than silently ignoring it.
    ///
    /// # Safety
    /// `base` is the SMMU MMIO base.
    unsafe fn global_error(&self) -> bool {
        // SAFETY: reading the error registers is side-effect free.
        unsafe { r32(self.base, GERROR) != r32(self.base, GERRORN) }
    }
}

/// Build a stage-2 translation table mapping `pages` from input `iova` to output
/// `pa` (IPA `iova + i*4K` → PA `pa + i*4K`) and nothing else, returning the root
/// (`S2TTB`). When `iova == pa` this is the identity map. On allocation failure it
/// frees whatever it built and returns `None`.
///
/// # Safety
/// `pa` is a page-aligned run of RAM in the linear map; `iova` is page-aligned;
/// `alloc` hands out zeroed frames from that map.
unsafe fn build_stage2_map<A: FrameAllocator>(
    iova: u64,
    pa: u64,
    pages: usize,
    alloc: &mut A,
) -> Option<u64> {
    // SAFETY: fresh zeroed frame from the linear map.
    let root = unsafe { alloc_table(alloc)? };
    for i in 0..pages {
        let off = (i * PAGE_SIZE) as u64;
        let (in_addr, out_addr) = (iova + off, pa + off); // walk by IOVA, map to PA
        let mut table = root;
        for level in 0..S2_LEAF_LEVEL {
            let idx = s2_index(in_addr, level);
            // SAFETY: `table` is a table frame in the linear map, `idx < 512`.
            let desc = unsafe { read_desc(table, idx) };
            table = if fmt::descriptor_is_valid(desc) {
                desc & S2_ADDR_MASK
            } else {
                // SAFETY: fresh zeroed frame for the next level.
                let Some(next) = (unsafe { alloc_table(alloc) }) else {
                    // SAFETY: `root` is the partial tree; frees only table frames.
                    unsafe { free_stage2_table(root, alloc) };
                    return None;
                };
                // SAFETY: valid table frame and index.
                unsafe { write_desc(table, idx, fmt::stage2_table(next)) };
                next
            };
        }
        // SAFETY: `table` is the level-3 table for `in_addr`, index in range.
        unsafe { write_desc(table, s2_index(in_addr, S2_LEAF_LEVEL), fmt::stage2_page(out_addr)) };
    }
    Some(root)
}

/// Free every table frame of the stage-2 tree rooted at `phys` (levels 0–3),
/// leaving the mapped buffer frames — which the tree only points at — untouched.
///
/// # Safety
/// `phys` is a stage-2 table root this module built; the tree has no block
/// descriptors above level 3 (identity maps are page-granular).
unsafe fn free_stage2_table<A: FrameAllocator>(phys: u64, alloc: &mut A) {
    // SAFETY: recursive helper; see its contract.
    unsafe { free_stage2_level(phys, 0, alloc) };
}

/// # Safety
/// `phys` is a level-`level` stage-2 table in the linear map.
unsafe fn free_stage2_level<A: FrameAllocator>(phys: u64, level: u8, alloc: &mut A) {
    if level < S2_LEAF_LEVEL {
        for idx in 0..512 {
            // SAFETY: `idx < 512`, `phys` is a table frame.
            let desc = unsafe { read_desc(phys, idx) };
            if fmt::descriptor_is_valid(desc) {
                // Levels above the leaf hold only table descriptors (no blocks).
                // SAFETY: the child is a level-(level+1) table in the linear map.
                unsafe { free_stage2_level(desc & S2_ADDR_MASK, level + 1, alloc) };
            }
        }
    }
    // Free this table frame itself (at every level, including the leaf table).
    alloc.free(staros_mm::PhysAddr(phys as usize));
}

/// Allocate one zeroed 4 KiB table frame from `alloc`.
///
/// # Safety
/// The returned frame is in the kernel's linear map.
unsafe fn alloc_table<A: FrameAllocator>(alloc: &mut A) -> Option<u64> {
    let frame = alloc.allocate()?.0 as u64;
    let base = phys_to_virt(frame) as *mut u64;
    for i in 0..512 {
        // SAFETY: `i < 512`, so the write stays within the 4 KiB frame.
        unsafe { base.add(i).write_volatile(0) };
    }
    Some(frame)
}

/// Write the eight doublewords of a StreamTableEntry for `streamid` into the
/// linear stream table.
///
/// # Safety
/// `strtab_phys` is the stream table in the linear map and `streamid` is within it.
unsafe fn write_ste(strtab_phys: u64, streamid: u32, ste: &[u64; fmt::STE_DWORDS]) {
    let base = phys_to_virt(strtab_phys) as *mut u64;
    let off = streamid as usize * fmt::STE_DWORDS;
    for (i, &w) in ste.iter().enumerate() {
        // SAFETY: `off + i` stays within the table (streamid range-checked by caller).
        unsafe { write_volatile(base.add(off + i), w) };
    }
}

/// # Safety
/// `table_phys` is a table frame in the linear map and `index < 512`.
unsafe fn read_desc(table_phys: u64, index: usize) -> u64 {
    // SAFETY: caller guarantees a valid table frame and in-range index.
    unsafe { read_volatile((phys_to_virt(table_phys) as *const u64).add(index)) }
}

/// # Safety
/// `table_phys` is a table frame in the linear map and `index < 512`.
unsafe fn write_desc(table_phys: u64, index: usize, desc: u64) {
    // SAFETY: caller guarantees a valid table frame and in-range index.
    unsafe { write_volatile((phys_to_virt(table_phys) as *mut u64).add(index), desc) };
}
