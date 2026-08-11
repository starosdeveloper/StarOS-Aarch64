//! STAR OS microkernel binary.
//!
//! This crate is the privileged core. It is deliberately thin: it wires the
//! portable subsystems (`mm`, `ipc`) to the arch backend and the HAL, then
//! enters the main loop. Everything that *can* live outside the kernel — device
//! drivers, filesystems, networking — is a user-space service reached over IPC,
//! not a module compiled in here. That separation is the whole point of the
//! rewrite.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use console::klog;
use core::fmt::Write;
use core::panic::PanicInfo;

use staros_abi::error::KError;
use staros_abi::syscall::Syscall;
use staros_arch_aarch64::addrspace::{self, AddressSpace};
use staros_arch_aarch64::gic::{self, GicSpec};
use staros_arch_aarch64::timer::{self, GenericTimer};
use staros_arch_aarch64::{exceptions, halt, mmu, psci, uart::Pl011, usermode};
use staros_fdt::Fdt;
use staros_framebuffer::{Console as FbConsole, Framebuffer, PixelFormat, Rgb};
use staros_hal::InterruptController;
use staros_mm::region::{self, Region};
use staros_mm::{FrameAllocator, FramePool, PhysAddr, PAGE_SIZE};

/// The `init` boot image: a separately-compiled EL0 program, produced by
/// `build.rs` as a real AArch64 **ELF64** executable. The kernel parses it and
/// maps each of its `PT_LOAD` segments — with the segment's own R/W/X rights —
/// into every user address space at runtime, rather than baking it into its own
/// `.text` or copying it verbatim as a flat blob.
static INIT_IMAGE: &[u8] = include_bytes!(env!("STAROS_INIT_IMAGE"));

/// The device-manager image: a separately-compiled EL0 program that, unlike
/// `init`, is real Rust linked against the `fdt` crate. It parses the device tree
/// in user space and mints device/interrupt capabilities for what it finds. Built
/// by `build.rs` as its own ELF; loaded into one process (the device manager)
/// rather than every user task.
static DEVICEMGR_IMAGE: &[u8] = include_bytes!(env!("STAROS_DEVICEMGR_IMAGE"));

/// The display server's image, built the same way. A separate program rather than
/// another role inside `init` because what distinguishes it is not a branch on an
/// id but a mapping nobody else is given — the screen.
static DISPLAYSRV_IMAGE: &[u8] = include_bytes!(env!("STAROS_DISPLAYSRV_IMAGE"));

/// The input driver's image. Also its own program: it links against the `virtio`
/// crate, and what it drives is a device the kernel has never heard of.
static INPUTSRV_IMAGE: &[u8] = include_bytes!(env!("STAROS_INPUTSRV_IMAGE"));

mod cap;
mod console;
mod elf;
mod heap;
mod iommu;
mod ipc;
mod irq;
mod mem;
mod notify;
mod obj;
mod sched;
mod smp;
mod sync;
mod syscall;

/// `#interrupt-cells` of every ARM interrupt controller: `<kind, number, flags>`.
const GIC_INTERRUPT_CELLS: u32 = 3;

/// Heap left over once the frame allocator's tree has taken its share: object
/// tables, task state, and whatever the kernel allocates as it grows.
const KERNEL_HEAP_SLACK: u64 = 1024 * 1024;

/// Timer tick rate driving preemption (Hz). Keeps preemption live under the
/// user-space processes even though this demo is IPC-driven.
const TICK_HZ: u64 = 10;

/// Maximum number of "do not touch this" regions we track while carving up the
/// memory map. QEMU `virt` declares none; a phone declares a few dozen.
const MAX_EXCLUSIONS: usize = 32;

/// The physical memory the kernel may actually use, and the RAM window overall.
struct MemoryMap {
    /// Full extent of RAM, `[start, end)` — what the MMU must map.
    ram: Region,
    /// Total usable bytes across all banks, for reporting.
    total: u64,
    /// The largest contiguous run nobody else has claimed.
    free: Region,
}

/// Everything in RAM that is already spoken for.
///
/// Getting this list wrong is not a subtle bug: hand the frame allocator the
/// device tree blob and it will be overwritten by a page table; hand it a
/// firmware carveout on a real device and the machine resets with no console to
/// say why. So the list is explicit and each entry is justified.
fn build_exclusions(fdt: &Fdt<'_>, dtb: u64, out: &mut [Region; MAX_EXCLUSIONS]) -> usize {
    let mut n = 0;
    let mut push = |r: Region| {
        if !r.is_empty() && n < MAX_EXCLUSIONS {
            out[n] = r;
            n += 1;
        }
    };

    // Us. `__image_end` covers .bss, the boot stack and everything else the
    // linker reserved, which is why the header advertises it as `image_size`.
    // The kernel is linked high, so these symbols are linear-map addresses;
    // everything on this list is physical, because that is what the frame
    // allocator hands out.
    extern "C" {
        static __image_start: u8;
        static __image_end: u8;
    }
    push(Region::from_bounds(
        mmu::virt_to_phys(&raw const __image_start as u64),
        mmu::virt_to_phys(&raw const __image_end as u64),
    ));

    // The device tree itself: we are still reading it, and will keep the
    // `Fdt` borrow alive for the rest of boot.
    push(Region::new(dtb, fdt.total_size() as u64));

    // The initrd, if the bootloader left one — on a real device this is how the
    // first user-space image arrives, so overwriting it would be self-defeating.
    if let Some((start, end)) = fdt.initrd() {
        push(Region::from_bounds(start, end));
    }

    // Firmware's own reservations, by both mechanisms the format offers.
    for (base, size) in fdt.memory_reservations() {
        push(Region::new(base, size));
    }
    for (base, size) in fdt.reserved_memory() {
        push(Region::new(base, size));
    }

    // There used to be a self-imposed exclusion here: the kernel was
    // identity-mapped and shared its address space with the EL0 window, so RAM at
    // the user window's addresses was unreachable while a user `TTBR0` was live,
    // and a whole gigabyte had to be written off. The kernel now runs in `TTBR1`
    // and reaches every frame through its own linear map, so the machine's memory
    // and user virtual addresses no longer compete for the same numbers.

    n
}

/// Work out what memory this machine has and which of it we may use.
fn survey_memory(fdt: &Fdt<'_>, dtb: u64) -> Option<MemoryMap> {
    let mut exclusions = [Region::default(); MAX_EXCLUSIONS];
    let count = build_exclusions(fdt, dtb, &mut exclusions);
    let exclusions = &exclusions[..count];

    let mut ram = Region::from_bounds(u64::MAX, 0);
    let mut total = 0u64;
    let mut free = Region::default();

    for (base, size) in fdt.memory()? {
        let bank = Region::new(base, size);
        if bank.is_empty() {
            continue;
        }
        ram.start = ram.start.min(bank.start);
        ram.end = ram.end.max(bank.end);
        total += bank.len();
        // Take the best window from each bank and keep the best overall. The
        // allocator manages one contiguous run for now; a bank-aware allocator
        // would use them all.
        if let Some(window) = region::largest_free(bank, exclusions) {
            let window = window.page_align_inward(PAGE_SIZE as u64);
            if window.len() > free.len() {
                free = window;
            }
        }
    }

    if ram.start >= ram.end || free.is_empty() {
        return None;
    }
    Some(MemoryMap { ram, total, free })
}

/// Bring up the heap and the frame allocator over the memory the machine has.
///
/// Order matters and is the whole trick: the frame allocator's tree is sized
/// from the pool, and lives on the heap — so the heap is carved off the front of
/// the same window, big enough to hold that tree plus room for the rest of the
/// kernel's dynamic state. Everything behind it becomes frames.
///
/// # Safety
/// Call once, after `mmu::init` has mapped `map.free` as writable Normal memory.
unsafe fn init_memory(console: &mut Pl011, map: &MemoryMap) {
    // Sized from `FramePool`, which decomposes the run into powers of two rather
    // than rounding it down to one — so the bill is a little under twice the frame
    // count, not a little under twice a *rounded-down* frame count. On a region
    // that is already a power of two the two agree, which is why this machine
    // never noticed the difference.
    let tree = FramePool::metadata_bytes(map.free.len() as usize) as u64;
    // Slack for everything else on the heap. Generous: unused heap is far
    // cheaper than an allocation failure in a kernel with nowhere to fail to.
    let heap_len = (tree + KERNEL_HEAP_SLACK).next_multiple_of(PAGE_SIZE as u64);

    let Some((heap, pool)) = map.free.split_at(heap_len) else {
        let _ = writeln!(
            console,
            "usable RAM ({} KiB) cannot even hold the kernel heap ({} KiB)",
            map.free.len() / 1024,
            heap_len / 1024
        );
        halt();
    };

    // SAFETY: `heap` is inside the free window (mapped writable by `mmu::init`),
    // excluded from the frame pool by construction, page-aligned and page-sized.
    // `heap::init` takes the address the allocator will *write* to, so the
    // physical run is handed over as its linear-map address.
    unsafe { heap::init(mmu::phys_to_virt(heap.start) as usize, heap.len() as usize) };

    if mem::init(PhysAddr(pool.start as usize), pool.len() as usize).is_err() {
        let _ = writeln!(console, "frame pool rejected: {:#x}..{:#x}", pool.start, pool.end);
        halt();
    }

    let managed = mem::with(|f| f.frames()) * PAGE_SIZE;
    let _ = writeln!(
        console,
        "memory: {} MiB RAM, {} MiB usable, heap {} KiB @ {:#x}, {} MiB of frames @ {:#x}",
        map.total / (1024 * 1024),
        map.free.len() / (1024 * 1024),
        heap.len() / 1024,
        heap.start,
        managed / (1024 * 1024),
        pool.start,
    );
}

/// Work out which interrupt controller this machine has, and where.
///
/// Binding by `compatible` is the whole point: `arm,gic-v3`'s first two `reg`
/// ranges are the distributor and the redistributor array, while a GICv2's are
/// the distributor and the memory-mapped CPU interface — same property, entirely
/// different meaning, and only the `compatible` string says which.
fn detect_gic(fdt: Fdt<'_>) -> Option<GicSpec> {
    if let Some(node) = fdt.find_compatible("arm,gic-v3") {
        let mut reg = node.reg()?;
        let (gicd, _) = reg.next()?;
        let (gicr, _) = reg.next()?;
        return Some(GicSpec::V3 { gicd: gicd as usize, gicr: gicr as usize });
    }
    // GICv2 is named after the first CPU that shipped one rather than after the
    // architecture; `arm,gic-400` is the other common spelling.
    let node = fdt
        .find_compatible("arm,cortex-a15-gic")
        .or_else(|| fdt.find_compatible("arm,gic-400"))?;
    let mut reg = node.reg()?;
    let (gicd, _) = reg.next()?;
    let (gicc, _) = reg.next()?;
    Some(GicSpec::V2 { gicd: gicd as usize, gicc: gicc as usize })
}

/// The SMMUv3's MMIO base, if the machine has one (`arm,smmu-v3` in the tree).
fn detect_smmu(fdt: Fdt<'_>) -> Option<u64> {
    let node = fdt.find_compatible("arm,smmu-v3")?;
    let (base, _) = node.reg()?.next()?;
    Some(base)
}

/// Bring up the IOMMU if the machine has one: allocate its tables and queues,
/// configure it default-deny, and report. A machine without an SMMU (plain
/// `virt`) simply skips this — the DMA-capability model is in place regardless,
/// but only an SMMU makes it enforceable against a real bus master.
/// Check the monotonic clock against the interval the tick source is armed with,
/// and say so in the log.
///
/// The two numbers come from the same register and nothing else: `interval` is
/// `CNTFRQ_EL0 / TICK_HZ` counter ticks, and the clock is `CNTPCT_EL0` scaled by
/// a fraction built from `CNTFRQ_EL0`. So waiting for the counter to advance by
/// exactly one tick period must take exactly one tick period on the clock. A
/// scale built wrong — the classic being an integer `ns_per_tick` that truncates,
/// or a frequency read from the wrong place — breaks that equality immediately,
/// while every other part of the system keeps working.
///
/// **What it deliberately does not check:** whether `CNTFRQ_EL0` tells the truth
/// about real time. Firmware that declares 100 MHz for a 54 MHz counter passes
/// this and is wrong by a factor of two; catching that needs a second clock (a
/// UART's baud rate, an HPET, a stopwatch), and there isn't one here.
///
/// An earlier version of this compared elapsed nanoseconds against *interrupts
/// counted* across the whole demo. It could not work: a tick is re-armed after it
/// is serviced, so under TCG the observed period ran 33 % long on a good run —
/// wide enough that a scale wrong by a factor of two sat comfortably inside the
/// tolerance. Removing the scheduler from the measurement is what makes the
/// tolerance tight enough to mean anything.
fn check_clock(console: &mut Pl011, interval_ticks: u64) {
    /// How far the measurement may stray from one tick period, either way. The
    /// wait is a spin on the counter itself, so the only real error is the
    /// overshoot of one loop iteration; 10 % is enormous headroom and still an
    /// order of magnitude tighter than a factor-of-two mistake.
    const TOLERANCE_PCT: u64 = 10;

    let Some(before) = timer::monotonic_ns() else {
        let _ = writeln!(console, "clock: no monotonic clock to check on this machine");
        return;
    };
    // Spin until the counter has advanced by one tick period. No interrupts, no
    // scheduler, no `wfi` — nothing in this loop can be delayed by anything the
    // rest of the kernel does, which is the whole point.
    let start = GenericTimer::counter();
    while GenericTimer::counter().wrapping_sub(start) < interval_ticks {
        core::hint::spin_loop();
    }
    let Some(after) = timer::monotonic_ns() else {
        let _ = writeln!(console, "clock: the monotonic clock stopped answering mid-check");
        return;
    };

    let measured_us = after.saturating_sub(before) / 1000;
    let expected_us = 1_000_000 / TICK_HZ;
    let low = expected_us * (100 - TOLERANCE_PCT) / 100;
    let high = expected_us * (100 + TOLERANCE_PCT) / 100;
    let verdict = if measured_us < low {
        "CLOCK SCALE WRONG - the clock under-counts nanoseconds"
    } else if measured_us > high {
        "CLOCK SCALE WRONG - the clock over-counts nanoseconds"
    } else {
        "agrees with the tick interval"
    };
    let _ = writeln!(
        console,
        "clock: one tick interval ({interval_ticks} counter ticks) measured {measured_us} us \
         against an expected {expected_us} us ({verdict})",
    );
}

/// How much time the monotonic clock saw pass while the demo ran, and how many
/// ticks core 0 took in it. Informational: this pairing cannot be a check (see
/// [`check_clock`]), but a clock that stopped, or a core that stopped being
/// preempted, both show up here as a zero.
fn report_clock(console: &mut Pl011, before: Option<u64>, ticks_before: u64) {
    let ticks = irq::tick_count_for(0).saturating_sub(ticks_before);
    let Some(elapsed_ns) = before.zip(timer::monotonic_ns()).map(|(a, b)| b.saturating_sub(a))
    else {
        let _ = writeln!(
            console,
            "clock: no monotonic clock on this machine — {ticks} tick(s) went unmeasured",
        );
        return;
    };
    let _ = writeln!(
        console,
        "clock: the demo took {} ms on the monotonic clock, during which core 0 took {ticks} tick(s)",
        elapsed_ns / 1_000_000,
    );

    // Sleeping, as three numbers that fail in different directions. Zero parks
    // would mean no task ever slept (so the state and the re-aimed timer went
    // untested); zero already-past would mean the "do not park a caller who is
    // already late" path never ran; and parks without wake-ups would mean tasks
    // went to sleep and were rescued by something other than the clock.
    let (parked, already_past, wakeups, worst_late) = sched::sleep_counts();
    // The overshoot is the sleep *resolution*: the gap between a deadline and the
    // task being made `Ready`, with no scheduling in it (a task timing its own
    // sleep would measure the round trip instead, and on a loaded run that says
    // more about contention than about the timer).
    //
    // It is **reported, not judged**, and that took two tries to accept. A verdict
    // with a threshold was written first, and it failed intermittently on machines
    // where the sleep was fine: the worst case is decided by whether the sleep
    // happened to land on the longest uninterruptible stretch in the kernel —
    // zeroing 2.5 MiB of `.bss` for a `Spawn`, or scrolling the framebuffer console
    // — and under TCG that is a lottery. Measured range across runs of the same
    // config: 2.9 ms to 437 ms. The typical value is single-digit milliseconds and
    // the number is worth printing; asserting on the worst case would be asserting
    // on the emulator's luck. A real board, where those stretches cost microseconds
    // rather than hundreds of milliseconds, is where this becomes a claim.
    let _ = writeln!(
        console,
        "sleep: {parked} task-sleep(s) parked, {already_past} deadline(s) already past (returned \
         at once), {wakeups} clock wake-up(s), worst overshoot {} us",
        worst_late / 1000,
    );
}

fn init_smmu(console: &mut Pl011, fdt: Fdt<'_>) {
    let Some(base) = detect_smmu(fdt) else {
        return;
    };
    let (sp, cp, ep) = staros_arch_aarch64::smmu::buffer_pages();
    // Allocate the three contiguous buffers and zero them — a zeroed stream table
    // is all-invalid entries, i.e. abort every stream, which is the default-deny
    // ground state we want.
    let bufs = mem::with(|f| {
        let strtab = f.alloc_pages(sp)?;
        let cmdq = f.alloc_pages(cp)?;
        let eventq = f.alloc_pages(ep)?;
        for (phys, pages) in [(strtab, sp), (cmdq, cp), (eventq, ep)] {
            // SAFETY: fresh contiguous run in the linear map, uniquely ours.
            unsafe {
                core::ptr::write_bytes(
                    mmu::phys_to_virt(phys.0 as u64) as *mut u8,
                    0,
                    pages * PAGE_SIZE,
                );
            }
        }
        let mk = |phys: PhysAddr, pages| staros_arch_aarch64::smmu::Buf { phys: phys.0 as u64, pages };
        Some(staros_arch_aarch64::smmu::Buffers {
            strtab: mk(strtab, sp),
            cmdq: mk(cmdq, cp),
            eventq: mk(eventq, ep),
        })
    });
    let Some(bufs) = bufs else {
        let _ = writeln!(console, "iommu: SMMUv3 present but out of memory for its tables");
        return;
    };
    // SAFETY: `base` is the SMMU MMIO base the tree reported (device linear map);
    // `bufs` are zeroed, contiguous allocations of the required sizes; runs once.
    match unsafe { staros_arch_aarch64::smmu::init(base, bufs) } {
        Ok((cfg, smmu)) => {
            let _ = writeln!(
                console,
                "iommu: SMMUv3 at {base:#x} (AIDR {:#x}) — {}-bit StreamIDs, stage1={} stage2={}, \
                 {} stream-table entries, default-abort, {}",
                cfg.aidr,
                cfg.sid_bits,
                cfg.s1p,
                cfg.s2p,
                1u32 << cfg.strtab_log2,
                if cfg.enabled { "ENABLED" } else { "enable NOT acknowledged" },
            );
            // Keep the handle so a task holding device authority can later bind a
            // DMA buffer to a device StreamID (roadmap 2.3 enforcement).
            iommu::install(smmu);
        }
        Err(_) => {
            let _ = writeln!(console, "iommu: SMMUv3 at {base:#x} did not initialise");
        }
    }
}

/// The PCIe ECAM base and the 32-bit memory window a BAR may live in, from the
/// `pci-host-ecam-generic` node. `None` if the machine has no PCIe host bridge.
///
/// The window comes from the bridge's `ranges`: entries of (child #address-cells=3,
/// parent #address-cells=2, size=2 cells); the child's high cell carries the space
/// code in bits [25:24], where `0b10` is 32-bit memory — the one a plain BAR decodes.
fn detect_pci(fdt: Fdt<'_>) -> Option<(u64, u64, u64)> {
    let node = fdt.find_compatible("pci-host-ecam-generic")?;
    let (ecam, _len) = node.reg()?.next()?;
    let ranges = node.property("ranges")?;
    let mut c = ranges.cells();
    loop {
        let child_hi = c.next()?;
        let (_child_mid, _child_lo) = (c.next()?, c.next()?);
        let (parent_hi, parent_lo) = (c.next()?, c.next()?);
        let (size_hi, size_lo) = (c.next()?, c.next()?);
        if (child_hi >> 24) & 0b11 == 0b10 {
            let base = (u64::from(parent_hi) << 32) | u64::from(parent_lo);
            let len = (u64::from(size_hi) << 32) | u64::from(size_lo);
            return Some((ecam, base, len));
        }
    }
}

/// Prove the SMMU actually translates a *real bus master*, end to end, with QEMU's
/// `edu` DMA device: bind its StreamID to a two-page buffer, DMA a pattern through
/// the mapped pages (must arrive) and then to an unmapped page (must be blocked).
///
/// This is the enforcement claim made falsifiable. Until now the SMMU was brought up
/// default-deny and `bind_stream` was proven only by the SMMU *accepting* its
/// commands (host tests pin the STE bits); nothing had ever watched a device emit a
/// transaction and be translated or aborted. `edu` is that device. Runs only when
/// the machine has both an SMMU and `edu` (add `-device edu`); otherwise a no-op.
fn smmu_dma_test(console: &mut Pl011, fdt: Fdt<'_>) {
    if detect_smmu(fdt).is_none() {
        return; // no SMMU: nothing gates the DMA, so the "blocked" half is meaningless
    }
    let Some((ecam, mmio_base, mmio_len)) = detect_pci(fdt) else {
        return;
    };
    // ECAM config space lives above RAM; map its linear-map block so we can read it.
    // SAFETY: `ecam` is the tree's ECAM base — real MMIO for this machine.
    unsafe { mmu::map_device_block(ecam) };

    const EDU_VENDOR: u16 = 0x1234;
    const EDU_DEVICE: u16 = 0x11e8;
    // SAFETY: ECAM is now mapped; the 32-bit window is a real PCI MMIO range the host
    // bridge decodes, and nothing else has claimed it this early.
    let Some(dev) = (unsafe {
        staros_arch_aarch64::pci::find_and_enable(ecam, EDU_VENDOR, EDU_DEVICE, mmio_base, mmio_len)
    }) else {
        return; // no edu attached; the SMMU is still up and default-deny
    };
    // SAFETY: `dev.bar0` is the BAR we just assigned, inside the device-mapped window.
    let edu = unsafe { staros_arch_aarch64::edu::Edu::new(dev.bar0) };
    if edu.liveness() != staros_arch_aarch64::edu::ID_MAGIC {
        let _ = writeln!(console, "iommu: edu present but its MMIO did not respond");
        return;
    }
    let sid = dev.bdf.stream_id();

    // Three contiguous pages: [ source | dest | forbidden ]. We map only the first
    // two into the device's IOVA space; the third is a real page it must *not* reach.
    let Some(buf) = mem::with(|f| f.alloc_pages(3)) else {
        return;
    };
    let buf_phys = buf.0 as u64;
    // The device addresses RAM by a *low* IOVA (edu masks DMA to 28 bits, and our RAM
    // sits above that) which the SMMU relocates to the high physical buffer — the
    // whole point of an IOMMU, and something an identity map could never show. Pages:
    // IOVA base -> source, +4K -> dest; +8K is deliberately left unmapped.
    const IOVA: u64 = 0x0020_0000;
    let (src_pa, dst_pa, forbidden_pa) = (buf_phys, buf_phys + 0x1000, buf_phys + 0x2000);
    let (src_iova, dst_iova, forbidden_iova) = (IOVA, IOVA + 0x1000, IOVA + 0x2000);
    const PATTERN: u32 = 0x5A5A_A5A5;
    const LEN: u64 = 256;

    // Seed the source; the forbidden page keeps its zeroed sentinel.
    // SAFETY: the buffer pages are RAM in the linear map, exclusively ours.
    unsafe {
        let s = mmu::phys_to_virt(src_pa) as *mut u32;
        for i in 0..(LEN as usize / 4) {
            core::ptr::write_volatile(s.add(i), PATTERN);
        }
    }

    // Map the two IOVA pages to the physical buffer for this device's StreamID.
    let bound = iommu::bind_at(sid, IOVA, buf_phys, 2).is_ok();

    let (mut allowed, mut blocked) = (false, false);
    if bound {
        // Allowed path: RAM(src) -> device buffer -> RAM(dst), both via mapped IOVAs.
        // SAFETY: edu is mapped; `LEN` is within its internal buffer.
        unsafe {
            edu.read_from_ram(src_iova, 0, LEN);
            edu.write_to_ram(0, dst_iova, LEN);
        }
        // SAFETY: reading the physical dest page back through the linear map.
        allowed = unsafe {
            let d = mmu::phys_to_virt(dst_pa) as *const u32;
            (0..(LEN as usize / 4)).all(|i| core::ptr::read_volatile(d.add(i)) == PATTERN)
        };

        // Blocked path: device buffer -> RAM at an unmapped IOVA — the SMMU aborts it.
        // SAFETY: as above; the write never lands, so the page keeps its sentinel.
        unsafe { edu.write_to_ram(0, forbidden_iova, LEN) };
        // SAFETY: reading the forbidden physical page — it must still be zero.
        blocked = unsafe {
            let d = mmu::phys_to_virt(forbidden_pa) as *const u32;
            (0..(LEN as usize / 4)).all(|i| core::ptr::read_volatile(d.add(i)) == 0)
        };
    }

    // Read the SMMU's own account of what happened. Until now the abort was proven
    // only by absence — the sentinel page stayed zero — which cannot distinguish
    // "the SMMU blocked it" from "the device never issued the transaction". The
    // event queue records *why*: the faulting StreamID, the address it emitted, and
    // the fault type. This is also the diagnostic that will matter on real hardware,
    // where there is no sentinel to inspect.
    let faulted = drain_smmu_events(console, sid, forbidden_iova);

    // Return the buffer frames so the teardown `every frame returned` check stays
    // honest; the stage-2 table is reclaimed at shutdown by `iommu::reclaim`.
    mem::with(|f| f.free_pages(buf));

    if bound && allowed && blocked {
        let _ = writeln!(
            console,
            "iommu: SMMU end-to-end via edu (StreamID {sid:#x}) - a bus master reached the \
             mapped page and was blocked from an unmapped one - translation enforced",
        );
    } else {
        let _ = writeln!(
            console,
            "iommu: edu DMA test INCONCLUSIVE (bound={bound} allowed={allowed} blocked={blocked})",
        );
    }
    if !faulted {
        // Not a failure of enforcement (the sentinel already proved that), but the
        // fault log is the diagnostic we will depend on when there is no sentinel.
        let _ = writeln!(
            console,
            "iommu: no fault record for the blocked access - event queue silent",
        );
    }
}

/// Drain the SMMU event queue and report whether the blocked access is among the
/// records: a fault for `sid` on the page of `want_addr`.
///
/// One forbidden DMA burst produces *many* records — `edu` copies in 4-byte beats and
/// each beat faults separately — so printing every record would bury the log in a
/// hundred near-identical lines. The first record is printed in full (that is the
/// diagnostic: type, stream, address, direction, stage) and the rest are counted.
/// Anything that does *not* match the expected fault is printed in full regardless,
/// because an unexpected fault is exactly what you want to see.
fn drain_smmu_events(console: &mut Pl011, sid: u32, want_addr: u64) -> bool {
    let page = |a: u64| a & !0xfff;
    let (mut matched, mut total) = (0u32, 0u32);
    // Bounded by the queue's own capacity: a wedged queue must not hang boot.
    for _ in 0..(1u32 << 8) {
        let Some(e) = iommu::next_event() else { break };
        total += 1;
        let expected = e.streamid == sid
            && (page(e.address) == page(want_addr) || page(e.ipa) == page(want_addr));
        if expected {
            matched += 1;
        }
        // Print the first record of the expected fault, and every unexpected one.
        if !expected || matched == 1 {
            let _ = writeln!(
                console,
                "iommu: fault record - {} (code {:#04x}) StreamID {:#x} at {:#x} (IPA {:#x}), \
                 {}, stage{}{}",
                e.kind.name(),
                e.code,
                e.streamid,
                e.address,
                e.ipa,
                if e.read { "read" } else { "write" },
                if e.stage2 { "2" } else { "1" },
                if expected { "" } else { " - UNEXPECTED" },
            );
        }
    }
    if total > 1 {
        let _ = writeln!(
            console,
            "iommu: {total} fault records drained ({matched} for the blocked page) - \
             one burst faults per beat",
        );
    }
    matched > 0
}

/// Ask the machine for a framebuffer, trying each source the tree advertises.
///
/// A real Raspberry Pi exposes a `brcm,bcm2835-mbox` and we drive the VideoCore
/// property mailbox; the QEMU `virt` machine has no mailbox but a `qemu,fw-cfg-mmio`
/// block through which we configure `ramfb`. Both yield the same
/// [`FramebufferInfo`]; the returned label only names which lit the screen. `None`
/// means neither is present (or the allocation/handshake failed) — the kernel then
/// runs on the UART alone.
fn acquire_framebuffer(
    fdt: &Fdt<'_>,
) -> Option<(staros_arch_aarch64::fbinfo::FramebufferInfo, &'static str)> {
    // Real hardware first: the VideoCore mailbox.
    if let Some(mbox) = fdt
        .find_compatible("brcm,bcm2835-mbox")
        .and_then(|node| node.reg()?.next())
        .map(|(base, _len)| base)
    {
        // SAFETY: `mbox` is the MMIO base the tree reported for the mailbox block, in
        // the device linear map; single-core early boot owns the registers. The
        // firmware-owned pixel buffer is permanently reserved.
        if let Some(info) = unsafe {
            staros_arch_aarch64::mailbox::init_framebuffer(mbox, 640, 480, |pages| {
                mem::with(|f| f.alloc_pages(pages))
            })
        } {
            return Some((info, "VideoCore mailbox"));
        }
    }

    // QEMU `virt`: ramfb over fw_cfg.
    let fw_cfg = fdt
        .find_compatible("qemu,fw-cfg-mmio")
        .and_then(|node| node.reg()?.next())
        .map(|(base, _len)| base)?;
    // SAFETY: `fw_cfg` is the MMIO base the tree reported for a fw_cfg block, in the
    // device linear map; single-core early boot owns the registers. The pixel buffer
    // is allocated here and deliberately never freed (accounted as reserved before
    // the free-run snapshot).
    let info = unsafe {
        staros_arch_aarch64::ramfb::init(fw_cfg, |pages| mem::with(|f| f.alloc_pages(pages)))
    }?;
    Some((info, "ramfb"))
}

/// Bring up a graphical console if the machine exposes a framebuffer.
///
/// The framebuffer's *source* is negotiated by [`acquire_framebuffer`] (VideoCore
/// mailbox on a Pi, `ramfb` on QEMU); from here down the path is identical — map the
/// pixels, wrap them in the portable console, install it, and run the font self-test.
/// If no source is present this quietly does nothing and the kernel runs UART-only.
fn init_framebuffer(console: &mut Pl011, fdt: &Fdt<'_>) -> Option<staros_arch_aarch64::fbinfo::FramebufferInfo> {
    let (info, source) = acquire_framebuffer(fdt)?;
    let len = info.height * info.stride;
    // SAFETY: the source returned a linear-mapped buffer of exactly `height * stride`
    // bytes that nothing else will touch; we hold it forever.
    let buf = unsafe {
        core::slice::from_raw_parts_mut(mmu::phys_to_virt(info.phys) as *mut u8, len)
    };
    // xRGB8888 matches QEMU ramfb's DRM_FORMAT_XRGB8888 exactly. On a real Pi the
    // byte order follows the mailbox SET_PIXEL_ORDER we requested; confirm it live
    // when the board arrives (a wrong choice only swaps R/B, not the addressing).
    let fb = Framebuffer::new(
        buf,
        info.width,
        info.height,
        info.stride,
        PixelFormat::xrgb8888(),
    )?;
    let mut screen = FbConsole::new(fb, Rgb::GREEN, Rgb::BLACK);
    // ASCII only: this line is drawn by the 8x8 font, which has no em-dash glyph.
    let _ = writeln!(screen, "STAR OS framebuffer console - {}x{}", info.width, info.height);
    console::install_framebuffer(screen);
    let _ = writeln!(
        console,
        "framebuffer: {source} {}x{} online (mirroring the console to the screen)",
        info.width, info.height,
    );
    // Prove the font renders cleanly with nothing else writing: this runs on the
    // primary core, before the secondaries or any EL0 task, so the sweep it prints
    // is race-free — a screenshot here shows the glyphs in isolation.
    console::framebuffer_selftest();
    Some(info)
}

/// Report what the machine said about itself.
///
/// Every line here is a fact the kernel currently hard-codes for QEMU `virt` and
/// will have to be *told* on a real board: where RAM is and how much, where the
/// UART and interrupt controller live, which GIC architecture the controller
/// actually implements, how to start the other cores. Printing them is how we
/// prove the parse is real — run QEMU with different `-m`/`-smp`/`gic-version`
/// and these numbers must follow, with no kernel rebuild.
fn describe_machine(console: &mut Pl011, fdt: &Fdt<'_>, dtb: u64) {
    let model = fdt
        .root()
        .ok()
        .and_then(|root| root.property("model")?.as_str())
        .unwrap_or("unknown");
    let _ = writeln!(
        console,
        "device tree at {dtb:#x} ({} bytes): {model}",
        fdt.total_size()
    );

    // RAM: the single most important thing the tree tells us. Today `mem::init`
    // uses a fixed 4 MiB pool carved out by the linker script; this is the
    // number that has to replace it.
    let mut total = 0u64;
    if let Some(banks) = fdt.memory() {
        for (base, size) in banks {
            total += size;
            let _ = writeln!(
                console,
                "  ram: {base:#x}..{:#x} ({} MiB)",
                base + size,
                size / (1024 * 1024)
            );
        }
    }
    let _ = writeln!(console, "  ram total: {} MiB", total / (1024 * 1024));

    // Carveouts. Empty on virt; on a phone this is a long list, and writing into
    // any of it is an instant silent reset.
    let reserved = fdt.memory_reservations().count() + fdt.reserved_memory().count();
    let _ = writeln!(console, "  reserved regions: {reserved}");

    // CPUs: the ids PSCI CPU_ON will take when SMP arrives.
    let cpus = fdt
        .find_node("/cpus")
        .map_or(0, |node| node.children().filter(|c| c.base_name() == "cpu").count());
    let _ = writeln!(console, "  cpus: {cpus} (booted on cpu {})", fdt.boot_cpuid());

    // The interrupt controller, by architecture — the same lookup that actually
    // selects the driver, so this line cannot drift from what gets installed.
    match detect_gic(*fdt) {
        Some(GicSpec::V3 { gicd, gicr }) => {
            let _ = writeln!(console, "  intc: GICv3, dist {gicd:#x}, redist {gicr:#x}");
        }
        Some(GicSpec::V2 { gicd, gicc }) => {
            let _ = writeln!(console, "  intc: GICv2, dist {gicd:#x}, cpu {gicc:#x}");
        }
        None => {
            let _ = writeln!(console, "  intc: none recognised");
        }
    }

    // How to start the other cores, and how to reboot — the latter being the
    // first thing to want on a device with no reset button.
    let psci = fdt
        .find_compatible("arm,psci-1.0")
        .or_else(|| fdt.find_compatible("arm,psci-0.2"));
    // The conduit is a property of *this boot*, not of the machine: the same
    // `virt` says "hvc" when we run under an emulated EL2 and "smc" when EL2 is
    // ours and PSCI moved up to EL3. So take it from the tree and never guess.
    match psci.and_then(|node| node.property("method")?.as_str()) {
        Some(method) => match psci::Conduit::from_dt(method) {
            Some(conduit) => {
                // SAFETY: single-core early boot; nothing has called PSCI yet.
                unsafe { psci::init(conduit) };
                let _ = writeln!(console, "  psci: present, method {method}");
            }
            None => {
                let _ = writeln!(
                    console,
                    "  psci: method \"{method}\" is not one we know; treating as absent"
                );
            }
        },
        None => {
            let _ = writeln!(console, "  psci: absent (cpu bring-up would need a spin table)");
        }
    }

    if let Some(uart) = fdt.find_compatible("arm,pl011") {
        if let Some((base, len)) = uart.reg().and_then(|mut r| r.next()) {
            let _ = writeln!(
                console,
                "  console: pl011 at {base:#x} (+{len:#x}) — this console, found not assumed"
            );
        }
    }
}

/// Kernel entry point, called from the arch boot stub (`rust_start`) once the
/// stack is up and `.bss` is zeroed.
///
/// The `#[no_mangle]` name matches the `extern "Rust" { fn kmain(dtb) }`
/// declaration in the arch crate, closing the boot handoff at link time. `dtb`
/// is the device tree pointer the bootloader left in `x0`.
#[no_mangle]
pub extern "Rust" fn kmain(dtb: u64) -> ! {
    // Ask the machine to describe itself *before* touching any hardware. The
    // parser needs no console, no heap and no MMU, which is what lets it be the
    // very first thing to run — and lets everything after it be told where the
    // hardware is instead of assuming.
    //
    // SAFETY: `dtb` is the *physical* pointer the bootloader placed in `x0` per
    // the boot contract, and the blob is read here at its linear-map address —
    // the boot stub mapped the blob's gigabyte as Normal memory precisely so this
    // read would work before anything else has run. Nothing has been allocated
    // over it yet (`mem::init` excludes it below). A bad pointer that still reads
    // as memory is rejected by `Fdt::new`'s validation; one that does not is a
    // broken bootloader we cannot defend against anyway.
    let machine = unsafe { Fdt::from_ptr(mmu::phys_to_virt(dtb) as *const u8) };

    // The console's address comes from the tree. If the blob is unusable we
    // fall back to the QEMU `virt` constant purely so there is *something* to
    // print the complaint on — on real hardware that fallback is a lie, and
    // the message below is the only warning we will get.
    let uart_base = machine
        .ok()
        .and_then(|fdt| Some(fdt.find_compatible("arm,pl011")?.reg()?.next()?.0 as usize))
        .unwrap_or(staros_arch_aarch64::uart::PL011_BASE);

    // The EL1 physical timer's line. The `arm,armv8-timer` binding declares four
    // in a fixed order — secure physical, non-secure physical, virtual, hypervisor
    // — and the second is the one this kernel arms.
    if let Some(intid) = machine.ok().and_then(|fdt| {
        let node = fdt.find_compatible("arm,armv8-timer")?;
        let (kind, number, _flags) = node.interrupts(GIC_INTERRUPT_CELLS)?.nth(1)?;
        gic::intid_from_dt(kind, number)
    }) {
        // SAFETY: early boot on the primary, before any core enables or arms a
        // timer.
        unsafe { timer::set_intid(intid) };
    }

    // The UART's interrupt line is no longer decoded here: the device manager
    // reads it from the same node in user space and mints the interrupt
    // capability itself (see `services/devicemgr`).

    // SAFETY: we are the single core executing early boot and nothing else has
    // touched the PL011 yet, so exclusive ownership of the MMIO region holds.
    // `uart_base` is the address the machine itself reported for a PL011.
    let mut console = unsafe { Pl011::new(uart_base) };

    let _ = writeln!(console);
    // A real bootloader hands off at EL2 (Linux asks for it, so KVM can use it),
    // so which level we were *entered* at is a property of the machine, not of
    // us. The boot stub records it before dropping to EL1, because afterwards
    // nothing can tell the two boots apart.
    let entered = staros_arch_aarch64::boot::entry_el();
    let _ = writeln!(
        console,
        "STAR OS microkernel v0.2.0 — entered at EL{entered}, running at EL{}",
        exceptions::current_el(),
    );

    match machine {
        Ok(fdt) => describe_machine(&mut console, &fdt, dtb),
        Err(e) => {
            let _ = writeln!(console, "device tree pointer x0={dtb:#x} is unusable: {e}");
            let _ = writeln!(console, "falling back to hard-coded QEMU virt addresses");
        }
    }

    let el = exceptions::current_el();
    if el != 1 {
        // The stub drops EL2 to EL1 for us, so getting here means we were handed
        // over at EL3 (or something stranger). Vectors, translation and the timer
        // all target EL1; don't pretend otherwise.
        let _ = writeln!(console, "not at EL1 (EL{el}) and the boot stub could not get us there; skipping bring-up");
        halt();
    }

    // SAFETY: at EL1, single-core, before any exception can occur.
    unsafe { exceptions::init() };
    let _ = writeln!(console, "exception vectors installed (VBAR_EL1)");

    // Turn on Privileged Access Never where the CPU has it (a real Cortex-A76 does,
    // QEMU's cortex-a72 does not). The kernel already reaches user memory only
    // through the unprivileged LDTR/STTR user-copy path, so this is defence in depth:
    // any accidental direct dereference of a user pointer now faults loudly.
    let pan = exceptions::enable_pan_if_supported();
    let _ = writeln!(
        console,
        "privileged access never (PAN): {}",
        if pan { "enabled" } else { "not implemented on this CPU" },
    );

    // Survey memory before enabling translation: the MMU has to map RAM, and
    // only the tree knows where it is and how much. Without a tree we do not
    // know what to map or what to allocate from, and guessing on real hardware
    // means overwriting firmware — so this is fatal rather than fudged.
    let Some(map) = machine.ok().and_then(|fdt| survey_memory(&fdt, dtb)) else {
        let _ = writeln!(console, "no usable RAM found in the device tree — cannot continue");
        halt();
    };

    // Replace the boot stub's provisional map with the real one. Translation has
    // been on since before Rust started — it had to be, since the kernel runs at
    // its high link addresses — but the stub could only guess: it marked the whole
    // low 512 GiB Device and made an exception for its own block and the tree's.
    // Now that the tree has been read, RAM becomes Normal and cacheable.
    // SAFETY: at EL1, runs exactly once, with the RAM window the device tree
    // reported. Every mapping in use right now keeps the attributes it already
    // has, so the rewrite cannot pull the ground out from under this code.
    unsafe { mmu::init(map.ram.start, map.ram.end) };
    let _ = writeln!(
        console,
        "MMU enabled: {} (kernel in TTBR1 at {:#x}; RAM {:#x}..{:#x} Normal in the linear map; \
         {}-bit PA per ID_AA64MMFR0_EL1)",
        mmu::is_enabled(),
        mmu::KERNEL_VA_OFFSET,
        map.ram.start,
        map.ram.end,
        mmu::pa_bits(),
    );

    // Now that RAM is mapped, carve the heap and the frame pool out of it.
    // SAFETY: `mmu::init` above mapped the whole RAM window writable; runs once.
    unsafe { init_memory(&mut console, &map) };

    // Exercise the heap to prove the global allocator is live before anything
    // depends on it.
    let squares: Vec<u32> = (1..=8).map(|n| n * n).collect();
    let sum: u32 = squares.iter().sum();
    let _ = writeln!(
        console,
        "kernel heap: Vec of {} squares (last {}) sums to {sum} — global allocator live",
        squares.len(),
        squares.last().copied().unwrap_or(0),
    );

    check_dynamic_tables(&mut console);

    // Bring up a framebuffer console if the machine offers one. On QEMU `virt`
    // this is `ramfb` over fw_cfg (present only with `-device ramfb`); on real
    // hardware a firmware framebuffer will take this slot. Done here, before the
    // frame pool's free run is snapshotted below, so the never-freed pixel buffer
    // is already excluded and does not read as a leak at teardown.
    // The geometry is kept: a display server needs it, and by the time one exists
    // the source it came from (mailbox or fw_cfg) is no longer around to ask.
    let framebuffer = machine.ok().and_then(|fdt| init_framebuffer(&mut console, &fdt));

    // Exercise the syscall path: `Yield` dispatches to 0, a bad number to -6.
    // SAFETY: vectors are installed; `invoke` issues a plain `svc`.
    let r_yield = unsafe { syscall::invoke(Syscall::Yield as usize, 0) };
    // SAFETY: same as above.
    let r_bad = unsafe { syscall::invoke(0xdead, 0) };
    let _ = writeln!(console, "syscall Yield -> {r_yield}; syscall 0xdead -> {r_bad}");

    // --- Monotonic clock ---
    // Started before the tick source, so the clock covers the rest of boot and so
    // the tick measurement below has a base older than the first tick. Until this
    // line runs, nothing in the system — kernel or EL0 — can measure elapsed time
    // at all; `ClockNow` answers `NotSupported` rather than zero.
    // SAFETY: primary core, during boot, before any other core is started.
    match unsafe { timer::init_monotonic() } {
        Some(scale) => {
            let _ = writeln!(
                console,
                "clock: {} Hz counter, {} ns per {} tick(s){}",
                GenericTimer::frequency_hz(),
                scale.numerator(),
                scale.denominator(),
                if scale.is_exact() { " (exact)" } else { "" },
            );
        }
        None => {
            // Firmware that never programmed CNTFRQ_EL0. Everything else still
            // works; timing does not, and says so once here rather than handing
            // out plausible numbers.
            let _ = writeln!(
                console,
                "clock: this machine reports a zero counter frequency — ClockNow will refuse",
            );
        }
    }

    // --- Timer + scheduler bring-up ---
    let interval = GenericTimer::frequency_hz() / TICK_HZ;
    irq::set_tick_interval(interval);
    // Which interrupt controller this machine has is a *runtime* fact, and the
    // two architectures are not remotely alike (see `arch::gic`). The kernel
    // reads it off the tree and hands the arch layer a `GicSpec`; nothing above
    // `hal::InterruptController` — `kernel::irq`, the notification bindings, the
    // user-space driver protocol — knows or cares which one it got.
    let spec = machine.ok().and_then(detect_gic).unwrap_or(GicSpec::V2 {
        gicd: gic::GICD_BASE,
        gicc: gic::GICC_BASE,
    });
    // SAFETY: at EL1 with vectors installed and IRQs still masked; `spec`
    // describes a controller the machine itself reported.
    match unsafe { gic::init(spec) } {
        Ok(()) => {
            let _ = writeln!(console, "interrupt controller: {} online", spec.version());
        }
        Err(_) => {
            let _ = writeln!(console, "failed to bring up {}", spec.version());
            halt();
        }
    }
    if gic::controller().enable(timer::intid()).is_err() {
        let _ = writeln!(console, "failed to route timer interrupt");
    }
    // Open the primary's wake doorbell too, so the cross-core IPI is symmetric
    // (every core can be signalled) before any core starts idling.
    smp::enable_wake_ipi();
    // SAFETY: timer IRQ routed; arm the periodic tick that drives preemption.
    unsafe { GenericTimer::arm(interval) };

    // With the interval known, hold the clock against it once — before any task
    // exists, so nothing the scheduler does can blur the measurement.
    check_clock(&mut console, interval);

    // --- IOMMU (SMMUv3), if the machine has one ---
    // Brought up default-deny before any driver could program a DMA-capable
    // device, so a bus master is confined from the start rather than after a race.
    if let Ok(fdt) = machine {
        init_smmu(&mut console, fdt);
        // With the SMMU up and default-deny, prove it enforces against a real bus
        // master (the `edu` PCIe DMA device), if one is attached.
        smmu_dma_test(&mut console, fdt);
    }

    // --- Wake the other cores ---
    // Only now: a secondary comes up through the *primary's* page tables, so they
    // have to exist and be final, and its stack comes out of `.bss`, which has to
    // be zeroed. Both are true by this point and neither was at the top of kmain.
    if let Ok(fdt) = machine {
        let cores = smp::start_secondaries(&mut console, &fdt);
        match psci::version() {
            Ok((major, minor)) => {
                let _ = writeln!(console, "smp: {cores} core(s) online (PSCI v{major}.{minor})");
            }
            Err(_) => {
                let _ = writeln!(console, "smp: {cores} core(s) online");
            }
        }
        // Four cores that set a bit and stopped would look identical to four
        // cores that work. Make them prove it, against each other, on a counter
        // that only a working lock can keep correct.
        smp::race_test(&mut console, cores);
        // And prove the primary can *signal* them, not just share memory: ring the
        // wake doorbell and confirm every secondary's handler ran. This is the same
        // IPI that later pulls idle cores out of `wfi` when work appears.
        smp::ipi_test(&mut console, cores);
    }

    // --- Build two isolated EL0 processes ---
    // Parse the separately-compiled `init` ELF once. Each process then gets its
    // own private copy of every `PT_LOAD` segment, mapped at the segment's own VA
    // with its own R/W/X rights; the spaces share no user frames. The id byte at
    // `USER_DATA_VA` (seeded below) selects which role of `init` each one runs:
    // 1 = client, 2 = resource server, 3 = UART-RX interrupt driver.
    let Some(image) = elf::Elf::parse(INIT_IMAGE) else {
        let _ = writeln!(console, "init image is not a valid AArch64 ELF64 executable");
        halt();
    };

    // The pool is untouched at this point. Remember how long its free run is, so
    // that after every task has exited we can ask whether it is that long again —
    // which it can only be if each space gave back every frame it ever took, down
    // to the last one, and the buddy tree coalesced them all.
    let free_run_before = mem::with(|frames| frames.largest_free_run());

    // Ids 1..=6 are the original demo roles; 9 is a storm sender (three of them
    // run at once) and 10 the storm receiver. Id 7 is the device manager, which is
    // a different program entirely, and 8 is what the spawner's children run.
    const STORM_SENDER_ID: u8 = 9;
    const STORM_RECV_ID: u8 = 10;
    // 11 walks its stack down page by page, forcing the kernel to grow it on
    // demand, then overruns the limit on purpose to prove the guard holds.
    const STACKGROW_ID: u8 = 11;
    // SAFETY: MMU is on with the frame pool mapped writable in the kernel's linear
    // map, so each freshly allocated frame is writable at `phys_to_virt(frame)`;
    // frames are uniquely owned by these calls.
    let [
        client_space,
        server_space,
        driver_space,
        canary_space,
        memtest_space,
        spawner_space,
        storm_a_space,
        storm_b_space,
        storm_c_space,
        storm_rx_space,
        stackgrow_space,
    ] = mem::with(|frames| unsafe {
        let mut spaces = [
            AddressSpace::new(frames).expect("client addrspace"),
            AddressSpace::new(frames).expect("server addrspace"),
            AddressSpace::new(frames).expect("driver addrspace"),
            AddressSpace::new(frames).expect("canary addrspace"),
            AddressSpace::new(frames).expect("memtest addrspace"),
            AddressSpace::new(frames).expect("spawner addrspace"),
            AddressSpace::new(frames).expect("storm sender a addrspace"),
            AddressSpace::new(frames).expect("storm sender b addrspace"),
            AddressSpace::new(frames).expect("storm sender c addrspace"),
            AddressSpace::new(frames).expect("storm receiver addrspace"),
            AddressSpace::new(frames).expect("stack grower addrspace"),
        ];
        for (i, space) in spaces.iter_mut().enumerate() {
            if !load_segments(space, frames, &image) {
                return None;
            }
            space.set_entry(image.entry());
            space.write_id(match i {
                0..=5 => i as u8 + 1,           // ids 1..=6
                6..=8 => STORM_SENDER_ID,       // three senders run the same role
                9 => STORM_RECV_ID,
                _ => STACKGROW_ID,
            });
        }
        Some(spaces)
    })
    .unwrap_or_else(|| {
        let _ = writeln!(console, "failed to load init image segments");
        halt()
    });

    // The device manager (id 7) is a *different* program — real Rust linked with
    // `fdt` — so it is built from its own image, and it alone receives the device
    // tree, mapped read-only into its space. It parses the tree to find hardware
    // rather than being handed pre-chosen addresses.
    let Some(dm_image) = elf::Elf::parse(DEVICEMGR_IMAGE) else {
        let _ = writeln!(console, "devicemgr image is not a valid AArch64 ELF64 executable");
        halt();
    };
    let dtb_len = machine.map(|f| f.total_size()).unwrap_or(0);
    // The initramfs the bootloader left, if any: physical base and length. It is
    // already excluded from the frame pool (`build_exclusions`), so mapping it is
    // safe. `None` (no `-initrd`) becomes length 0, which the process reads as "no
    // initramfs".
    let initrd = machine.ok().and_then(|f| f.initrd());
    // SAFETY: the MMU is on with the frame pool mapped writable in the kernel's
    // linear map, so each freshly allocated frame is writable at its linear-map
    // address; the frames are uniquely owned by these calls, and `dtb`'s blob is
    // the real tree the kernel just parsed.
    let devicemgr_space = mem::with(|frames| unsafe {
        let mut s = AddressSpace::new(frames)?;
        if !load_segments(&mut s, frames, &dm_image) {
            s.destroy(frames);
            return None;
        }
        s.set_entry(dm_image.entry());
        s.write_id(7);
        // Map the blob read-only into this process and seed where it landed, so
        // the manager can parse the same tree the kernel did.
        let Some(dtb_va) = s.map_dtb(frames, dtb, dtb_len) else {
            s.destroy(frames);
            return None;
        };
        s.write_dtb_info(dtb_va, dtb_len as u32);
        // Likewise the initramfs, if present: map it read-only and seed where it
        // landed so the process can unpack the CPIO archive in user space.
        if let Some((start, end)) = initrd {
            let len = (end - start) as usize;
            let Some(initrd_va) = s.map_initrd(frames, start, len) else {
                s.destroy(frames);
                return None;
            };
            s.write_initrd_info(initrd_va, len as u32);
        }
        Some(s)
    })
    .unwrap_or_else(|| {
        let _ = writeln!(console, "failed to load devicemgr image");
        halt()
    });
    let _ = writeln!(
        console,
        "loaded init ELF: {} bytes, entry {:#x}",
        INIT_IMAGE.len(),
        image.entry()
    );

    let _ = writeln!(
        console,
        "scheduler: capability delegation (client + server) + user-space IRQ driver"
    );

    // Endpoints carry all the IPC in the demo. ep0/ep1 are the client<->server
    // request and reply; ep2/ep3 carry the device manager's grants to the driver
    // and the server respectively.
    let ep0 = obj::create(obj::Object::Endpoint { id: 0 }).expect("ep0 object");
    let ep1 = obj::create(obj::Object::Endpoint { id: 1 }).expect("ep1 object");
    let ep_drv = obj::create(obj::Object::Endpoint { id: 2 }).expect("ep_drv object");
    let ep_srv = obj::create(obj::Object::Endpoint { id: 3 }).expect("ep_srv object");
    // The contention endpoint: three senders and one receiver, all on it at once.
    let ep_storm = obj::create(obj::Object::Endpoint { id: ipc::STORM_EP }).expect("ep_storm object");
    // The display protocol: a client's commit request and the server's reply.
    let ep_fb = obj::create(obj::Object::Endpoint { id: 5 }).expect("ep_fb object");
    let ep_fb_reply = obj::create(obj::Object::Endpoint { id: 6 }).expect("ep_fb_reply object");
    // The device manager's grants to the input driver.
    let ep_input = obj::create(obj::Object::Endpoint { id: 7 }).expect("ep_input object");

    // The *only* device policy the kernel still holds: the authority to mint. It
    // pre-mints no UART objects at all now — the device manager (id 7) reads the
    // tree, mints a device object for the driver, a separate one for the server
    // (so the server revoking its own cannot disturb the driver's), and the
    // interrupt object, all from this authority, and delegates them over ep2/ep3.
    let authority = obj::create(obj::Object::DeviceAuthority).expect("device authority object");

    // Grant each process only the capabilities it needs, named by handle in its
    // own table. Endpoint 0 carries client->server requests, endpoint 1 the
    // replies. The *server* holds a UART device capability (which it delegates to
    // the client then revokes); the *driver* holds its own UART device capability
    // plus the interrupt capability for the UART line.
    // Granting assigns handles bottom-up from 1, so the order of these calls *is*
    // the handle numbering the programs were built against — the slot indices
    // this used to write by hand are now the consequence of the grants rather
    // than a second place to keep in sync with them.
    let mut client_caps = cap::empty_caps().expect("client caps");
    cap::install(&mut client_caps, cap::Cap::Endpoint { obj: ep0, send: true, recv: false });
    cap::install(&mut client_caps, cap::Cap::Endpoint { obj: ep1, send: false, recv: true });
    // handle 3 is left free — it will hold the UART capability the server delegates.

    // Server: the two client channels plus a receive endpoint (handle 3) on which
    // the device manager delegates its UART device capability. It no longer holds
    // a pre-minted device object; it receives one and installs it on handle 4.
    let mut server_caps = cap::empty_caps().expect("server caps");
    cap::install(&mut server_caps, cap::Cap::Endpoint { obj: ep0, send: false, recv: true });
    cap::install(&mut server_caps, cap::Cap::Endpoint { obj: ep1, send: true, recv: false });
    cap::install(&mut server_caps, cap::Cap::Endpoint { obj: ep_srv, send: false, recv: true });

    // Driver: a single receive endpoint (handle 1) on which the device manager
    // delegates the UART device capability (installed on handle 2) and then the
    // interrupt capability (handle 3). Nothing about the device is hard-coded here.
    let mut driver_caps = cap::empty_caps().expect("driver caps");
    cap::install(&mut driver_caps, cap::Cap::Endpoint { obj: ep_drv, send: false, recv: true });

    // Canary: a task granted *nothing*. It deliberately touches kernel memory
    // from EL0 to prove fault isolation — the kernel kills it and keeps running
    // the others, instead of the whole system going down.
    let canary_caps = cap::empty_caps().expect("canary caps");

    // Memtest: also granted nothing. It grows its own memory with `MapAnon`
    // (which needs no capability), writes and reads it back, and reports success
    // via the unprivileged `DebugPutc`.
    let memtest_caps = cap::empty_caps().expect("memtest caps");

    // Spawner: a minimal `init`-style root. It uses the `Spawn` syscall to create
    // a *child* process at runtime — the kernel no longer hard-codes every task.
    let spawner_caps = cap::empty_caps().expect("spawner caps");

    // Device manager: holds the device authority and nothing else. It parses the
    // device tree (mapped read-only into its space) to find the UART, then mints a
    // device and an interrupt capability for the address *it* discovered via
    // `GrantDevice`/`GrantIrq`, and drives the UART through the minted capability.
    // The kernel chose neither the address nor the line — only who may turn them
    // into capabilities.
    let mut devicemgr_caps = cap::empty_caps().expect("devicemgr caps");
    cap::install(&mut devicemgr_caps, cap::Cap::DeviceAuthority { obj: authority });
    cap::install(&mut devicemgr_caps, cap::Cap::Endpoint { obj: ep_drv, send: true, recv: false });
    cap::install(&mut devicemgr_caps, cap::Cap::Endpoint { obj: ep_srv, send: true, recv: false });
    cap::install(&mut devicemgr_caps, cap::Cap::Endpoint { obj: ep_input, send: true, recv: false });

    // The storm tasks: each sender gets send-only, the receiver recv-only, on the
    // one shared endpoint — handle 1 in every case, which is what the role code in
    // `image.rs` is written against. Nothing else is granted: a task that can only
    // send cannot drain the ring it is contending for.
    let storm_send_caps = || {
        let mut caps = cap::empty_caps().expect("storm sender caps");
        cap::install(&mut caps, cap::Cap::Endpoint { obj: ep_storm, send: true, recv: false });
        caps
    };
    let (storm_a_caps, storm_b_caps, storm_c_caps) =
        (storm_send_caps(), storm_send_caps(), storm_send_caps());
    let mut storm_rx_caps = cap::empty_caps().expect("storm receiver caps");
    cap::install(&mut storm_rx_caps, cap::Cap::Endpoint { obj: ep_storm, send: false, recv: true });

    // The display server and its one client, but only on a machine that has a
    // screen to give away. Everything about this pair is ordinary — two processes
    // and two endpoints — except that one of them is handed the pixels.
    let display = framebuffer.and_then(|info| {
        let len = info.height * info.stride;
        // SAFETY: as the other spaces here; `info` describes the real pixel buffer,
        // which `build_exclusions` kept out of the frame pool, and this is the only
        // space it is mapped into.
        let space = mem::with(|frames| unsafe {
            let mut s = AddressSpace::new(frames)?;
            let ds_image = elf::Elf::parse(DISPLAYSRV_IMAGE)?;
            if !load_segments(&mut s, frames, &ds_image) {
                s.destroy(frames);
                return None;
            }
            s.set_entry(ds_image.entry());
            s.write_id(14);
            let Some(fb_va) = s.map_framebuffer(frames, info.phys, len) else {
                s.destroy(frames);
                return None;
            };
            s.write_fb_info(fb_va, info.width as u32, info.height as u32, info.stride as u32);
            Some(s)
        })?;
        let mut caps = cap::empty_caps()?;
        cap::install(&mut caps, cap::Cap::Endpoint { obj: ep_fb, send: false, recv: true });
        cap::install(&mut caps, cap::Cap::Endpoint { obj: ep_fb_reply, send: true, recv: false });

        // Its client: an ordinary `init` role with no privilege at all beyond the
        // two endpoint capabilities. It cannot reach the screen; it can only ask.
        // SAFETY: as every other space built here — the MMU is on with the frame
        // pool identity-mapped and writable, and these frames are uniquely ours.
        let client = mem::with(|frames| unsafe {
            let mut s = AddressSpace::new(frames)?;
            if !load_segments(&mut s, frames, &image) {
                s.destroy(frames);
                return None;
            }
            s.set_entry(image.entry());
            s.write_id(13);
            Some(s)
        })?;
        let mut client_caps = cap::empty_caps()?;
        cap::install(&mut client_caps, cap::Cap::Endpoint { obj: ep_fb, send: true, recv: false });
        cap::install(
            &mut client_caps,
            cap::Cap::Endpoint { obj: ep_fb_reply, send: false, recv: true },
        );
        Some(((space, caps), (client, client_caps)))
    });

    // Hand the screen over *before* either of them runs. From here the kernel logs
    // to the UART only; a panic takes the screen back (`console::reclaim_framebuffer`)
    // because a fault report nobody can see is a fault report that did not happen.
    if display.is_some() {
        console::stop_mirroring();
        let _ = writeln!(
            console,
            "framebuffer: handed to displaysrv (id 14); the kernel logs to the UART from here"
        );
    }

    // The input driver. Built unconditionally — whether the machine *has* an input
    // device is not the kernel's business to know: the driver receives a capability
    // or it does not, and either way the kernel's part is the same three
    // primitives. It holds nothing but the endpoint it receives on.
    let input = (|| {
        // SAFETY: as every other space built here.
        let space = mem::with(|frames| unsafe {
            let mut s = AddressSpace::new(frames)?;
            let img = elf::Elf::parse(INPUTSRV_IMAGE)?;
            if !load_segments(&mut s, frames, &img) {
                s.destroy(frames);
                return None;
            }
            s.set_entry(img.entry());
            s.write_id(15);
            Some(s)
        })?;
        let mut caps = cap::empty_caps()?;
        cap::install(&mut caps, cap::Cap::Endpoint { obj: ep_input, send: false, recv: true });
        Some((space, caps))
    })();

    // Remember the client's root table frame; after the tasks exit, teardown
    // returns it to the buddy allocator, and the next allocation should hand that
    // very frame back — visible proof the space was reclaimed, not leaked.
    let client_root = client_space.ttbr0();

    sched::spawn_user(user_task_entry, client_space, client_caps);
    sched::spawn_user(user_task_entry, server_space, server_caps);
    sched::spawn_user(user_task_entry, driver_space, driver_caps);
    sched::spawn_user(user_task_entry, canary_space, canary_caps);
    sched::spawn_user(user_task_entry, memtest_space, memtest_caps);
    sched::spawn_user(user_task_entry, spawner_space, spawner_caps);
    // Before the device manager, so it is already blocked in `Recv` when the
    // manager delegates — otherwise the grants queue up and the ordering, rather
    // than the wake path, is what makes the demo work.
    if let Some((space, caps)) = input {
        sched::spawn_user(user_task_entry, space, caps);
    }
    sched::spawn_user(user_task_entry, devicemgr_space, devicemgr_caps);
    // The receiver goes in first so it is already blocked in `Recv` when the
    // senders start: the wake path is then part of what is being tested, not an
    // artefact of ordering.
    sched::spawn_user(user_task_entry, storm_rx_space, storm_rx_caps);
    // The display server first, so it is already blocked in `Recv` when its client
    // commits — the wake path is then part of what runs, not an artefact of order.
    if let Some(((ds_space, ds_caps), (fbc_space, fbc_caps))) = display {
        sched::spawn_user(user_task_entry, ds_space, ds_caps);
        sched::spawn_user(user_task_entry, fbc_space, fbc_caps);
    }
    sched::spawn_user(user_task_entry, storm_a_space, storm_a_caps);
    sched::spawn_user(user_task_entry, storm_b_space, storm_b_caps);
    sched::spawn_user(user_task_entry, storm_c_space, storm_c_caps);
    // Granted nothing: growing a stack needs no capability, and neither does
    // running off the end of one.
    sched::spawn_user(
        user_task_entry,
        stackgrow_space,
        cap::empty_caps().expect("stack grower caps"),
    );
    // Bracket the whole demo with the monotonic clock and the tick counter. Two
    // independent readings of the same elapsed time: one from `CNTPCT_EL0` scaled
    // to nanoseconds, one from counting interrupts armed at `freq / TICK_HZ`
    // ticks apart. They are derived from the same counter but through entirely
    // different arithmetic, so a wrong tick→nanosecond scale makes them disagree
    // — which is the only thing that can catch a scale that is confidently wrong.
    let clock_before = timer::monotonic_ns();
    let ticks_before = irq::tick_count();

    // Runs the tasks (each starts with IRQs enabled) until they exit — including
    // any the spawner creates at runtime via `Spawn`.
    sched::start();

    // SAFETY: tasks are done; stop the timer source.
    unsafe { GenericTimer::disable() };

    report_clock(&mut console, clock_before, ticks_before);

    // Reclaim shared-memory and DMA frames now that no task can still map them.
    // These belong to their objects, not to any address space, so task teardown
    // left them alone — this is what lets the "every frame returned" check below
    // still hold once tasks have used shared or DMA buffers.
    mem::with(obj::free_reclaimable);
    // And the stage-2 tables the SMMU binds hung off — those are ours, not any
    // task's, and would otherwise show up as leaked frames.
    iommu::reclaim();

    let _ = writeln!(
        console,
        "scheduler: all tasks finished after {} timer ticks; task table grew to {} \
         (old fixed max 8)",
        irq::tick_count(),
        sched::task_count(),
    );

    // Dead-task stacks are reclaimed by each exiting task's successor, not left
    // until reboot. A non-zero count here is the proof: without reaping this line
    // would read zero and the 32 KiB stacks would have leaked.
    let (reaped, reaped_bytes) = sched::reaped_stacks();
    let _ = writeln!(
        console,
        "task teardown: reaped {reaped} dead-task kernel stacks ({} KiB returned to the heap)",
        reaped_bytes / 1024,
    );

    // Stack pages that arrived because a task touched them, not because the kernel
    // guessed. Zero here would mean every stack was already big enough up front —
    // i.e. demand paging never ran — so the number is the claim, not the feature.
    let grown = sched::stack_pages_grown();
    let _ = writeln!(
        console,
        "user stacks: {grown} page(s) mapped on demand ({} KiB), {} mapped up front per task, \
         limit {} KiB",
        grown * PAGE_SIZE as u64 / 1024,
        addrspace::USER_STACK_PAGES,
        addrspace::USER_STACK_MAX_PAGES * PAGE_SIZE as u64 / 1024,
    );

    // Which cores were actually preempted, and how often. This is the only thing
    // in the log that can tell a core running tasks *preemptively* from one
    // running them until they happen to make a syscall: a secondary whose GIC
    // interface or timer never came up contributes a flat zero here while the
    // total still climbs.
    let mut ticks = alloc::string::String::new();
    for cpu in 0..staros_arch_aarch64::boot::MAX_CPUS {
        let n = irq::tick_count_for(cpu);
        if n > 0 {
            let _ = core::fmt::Write::write_fmt(&mut ticks, format_args!(" cpu{cpu}={n}"));
        }
    }
    let _ = writeln!(console, "preemption: timer ticks per core —{ticks}");

    // The IPC contention test's other half. The receiver already reported that the
    // *arithmetic* held (no message lost or duplicated); this says the traffic was
    // genuinely spread across cores rather than serialised on one — the difference
    // between exercising an endpoint and exercising it concurrently. `race_test`
    // makes the same claim for the kernel's lock; this makes it for the endpoint's
    // ring, wait queues, and block/wake path.
    let (sends, recvs, send_cores, recv_cores) = ipc::storm_stats();
    let mut spread = alloc::string::String::new();
    for cpu in 0..staros_arch_aarch64::boot::MAX_CPUS {
        if sends[cpu] > 0 || recvs[cpu] > 0 {
            let _ = core::fmt::Write::write_fmt(
                &mut spread,
                format_args!(" cpu{cpu}={}s/{}r", sends[cpu], recvs[cpu]),
            );
        }
    }
    let total_sends: u64 = sends.iter().sum();
    let total_recvs: u64 = recvs.iter().sum();
    // On a single-core machine the traffic is necessarily serialised and that is not
    // a failure — the arithmetic check still applies. The verdict only claims
    // concurrency where there were cores to be concurrent on.
    let verdict = if smp::online_count() > 1 && send_cores > 1 {
        "endpoint contended across cores"
    } else {
        "endpoint exercised on one core"
    };
    let _ = writeln!(
        console,
        "ipc storm: {total_sends} sends / {total_recvs} recvs on one endpoint —{spread} \
         ({send_cores} core(s) sending, {recv_cores} receiving) — {verdict}",
    );

    // Every address space has been torn down. Two questions, and the second is the
    // one that matters.
    //
    // Did *a* frame come back? Allocate one and show it is the exact frame the
    // client's root table occupied.
    if let Some(PhysAddr(reused)) = mem::alloc_frame() {
        let _ = writeln!(
            console,
            "frame reclaim: post-teardown alloc {reused:#x} (exited client's root was {client_root:#x})"
        );
        // Give it straight back, so it does not skew the whole-pool check below.
        mem::with(|frames| frames.free(PhysAddr(reused)));
    }

    // Did *every* frame come back? Seven processes each mapped a 3 MiB image, and
    // one of them grew a 16 MiB heap on top — thousands of frames and the page
    // tables to reach them, none of it tracked anywhere but in the tables
    // themselves. If the free run is as long as it was before any of that, the
    // tree walk found all of it. One leaked frame would split the run and show up
    // here as a smaller number.
    //
    // The scheduler stops when nothing is *runnable*, which is not the same as
    // nothing being alive: with no console input the UART driver is still blocked
    // in `Wait`, and the frames it is still holding are not a leak. So say which
    // it is rather than accusing the allocator.
    let free_run_after = mem::with(|frames| frames.largest_free_run());
    let still_held = sched::live_spaces();
    let verdict = if free_run_after == free_run_before {
        "every frame returned"
    } else if still_held > 0 {
        "short only by what the tasks below still hold"
    } else {
        "LEAKED"
    };
    let _ = writeln!(
        console,
        "frame reclaim: longest free run {} MiB -> {} MiB after teardown — {verdict}",
        free_run_before * PAGE_SIZE / (1024 * 1024),
        free_run_after * PAGE_SIZE / (1024 * 1024),
    );
    if still_held > 0 {
        let _ = writeln!(
            console,
            "  ({still_held} task(s) still alive and holding their address space — \
             send a newline to let the UART driver exit and the pool returns whole)"
        );
    }

    // The demo is over, so put the machine down rather than spinning forever.
    // This is PSCI `SYSTEM_OFF` — the same interface that started the other
    // cores, and on a real device the only way to power off or reboot something
    // whose buttons you cannot reach. If firmware declines, fall back to halting.
    let _ = writeln!(console, "shutting down (PSCI SYSTEM_OFF)");
    psci::system_off();
    let _ = writeln!(console, "PSCI declined to power off; halting instead");
    halt();
}

/// Build a process from an ELF image sitting in the *calling* task's memory, seed
/// it with `id`, and schedule it. Returns the new task's id, or a negative
/// [`KError`]. Backs the `SpawnImage` syscall.
///
/// This is what makes a program a *file* rather than something the kernel shipped.
/// `Spawn` can only start another copy of the one image compiled into the kernel;
/// here the caller supplies the bytes, so a process can come out of an initramfs,
/// off a network, or from a compiler that just finished — and the kernel still has
/// no idea what an archive or a filesystem is.
///
/// The image is copied into the kernel heap before it is parsed, which is the
/// honest reason for the size cap at the syscall. Two things force the copy: the
/// ELF parser wants a contiguous slice, and reading EL0 memory directly from EL1
/// faults under PAN on real hardware. Streaming each segment through a page-sized
/// bounce buffer would lift the cap; nothing yet needs it.
///
/// A capability-less child, exactly like [`spawn_child`]: it earns authority only
/// by being sent it.
pub fn spawn_image(ptr: u64, len: usize, id: u8) -> isize {
    let mut bytes = alloc::vec::Vec::new();
    if bytes.try_reserve_exact(len).is_err() {
        return KError::OutOfResources.as_raw();
    }
    bytes.resize(len, 0);
    // SAFETY: the caller's address space is active and the syscall layer confirmed
    // every byte of `[ptr, ptr + len)` is mapped EL0-readable in its own tables;
    // `bytes` is exactly `len` long. `copy_from_user` uses unprivileged loads, so
    // this is sound under PAN.
    unsafe { staros_arch_aarch64::usercopy::copy_from_user(&mut bytes, ptr) };

    let Some(image) = elf::Elf::parse(&bytes) else {
        return KError::InvalidArgument.as_raw();
    };
    // SAFETY: as `spawn_child` — the MMU is on with the frame pool identity-mapped
    // and writable, and every frame allocated here is uniquely owned.
    let space = mem::with(|frames| unsafe {
        let mut s = AddressSpace::new(frames)?;
        if !load_segments(&mut s, frames, &image) {
            s.destroy(frames);
            return None;
        }
        s.set_entry(image.entry());
        s.write_id(id);
        Some(s)
    });
    let Some(space) = space else {
        return KError::OutOfResources.as_raw();
    };
    let Some(caps) = cap::empty_caps() else {
        // SAFETY: `space` was just built and never installed as a live TTBR0.
        mem::with(|frames| unsafe { space.destroy(frames) });
        return KError::OutOfResources.as_raw();
    };
    match sched::spawn_user(user_task_entry, space, caps) {
        Some(task_id) => task_id as isize,
        None => {
            // SAFETY: as above — the space never became anyone's active tables.
            mem::with(|frames| unsafe { space.destroy(frames) });
            KError::OutOfResources.as_raw()
        }
    }
}

/// Build a fresh EL0 process from the init image, seeded with `id`, and add it to
/// the scheduler with no capabilities. Backs the `Spawn` syscall, letting user
/// space grow the process tree itself. Returns `false` if the image is
/// unparseable, the frame pool is exhausted, or the scheduler is full.
pub fn spawn_child(id: u8) -> bool {
    let Some(image) = elf::Elf::parse(INIT_IMAGE) else {
        return false;
    };
    // SAFETY: MMU is on with the frame pool identity-mapped and writable, so each
    // freshly allocated frame is writable here at VA == PA and uniquely owned.
    let space = mem::with(|frames| unsafe {
        let mut s = AddressSpace::new(frames)?;
        if !load_segments(&mut s, frames, &image) {
            s.destroy(frames);
            return None;
        }
        s.set_entry(image.entry());
        s.write_id(id);
        Some(s)
    });
    let Some(space) = space else {
        return false;
    };
    // A child is granted nothing; it earns capabilities only by being sent them.
    // This is a syscall path, so an exhausted heap has to come back as `false`.
    let caps = match cap::empty_caps() {
        Some(caps) => caps,
        None => {
            // SAFETY: `space` was just built and never installed as a live TTBR0.
            mem::with(|frames| unsafe { space.destroy(frames) });
            return false;
        }
    };
    if sched::spawn_user(user_task_entry, space, caps).is_some() {
        true
    } else {
        // The task could not be allocated: return the frames rather than leak them.
        // SAFETY: `space` was just built and never installed as a live TTBR0.
        mem::with(|frames| unsafe { space.destroy(frames) });
        false
    }
}

/// Prove the kernel's tables actually grow, by asking for far more than the
/// fixed arrays they replaced would ever have handed out.
///
/// The demo cannot show this on its own: it needs five objects and three
/// capabilities, so it ran identically on the old `MAX_OBJECTS = 8` /
/// `MAX_CAPS = 4` arrays and would keep passing if this change had done nothing.
/// A check is only worth printing if the old code would have failed it, so this
/// asks for 64 of each — the ninth object and the fifth capability were flatly
/// impossible before.
fn check_dynamic_tables(console: &mut Pl011) {
    /// Comfortably past every old limit, and small enough to give straight back.
    const N: usize = 64;

    // Objects. Keep the references: resolving the *last* one is what shows the
    // table grew, rather than wrapping or quietly dropping the excess.
    let mut refs = Vec::new();
    for i in 0..N {
        let Some(r) = obj::create(obj::Object::Endpoint { id: i }) else {
            let _ = writeln!(console, "dynamic tables: object {i} FAILED — table did not grow");
            return;
        };
        refs.push(r);
    }
    let objects_ok = obj::get(refs[N - 1]).is_some();

    // Capabilities in a single task's table. Handles are assigned bottom-up from
    // 1, so the Nth grant must come back as handle N and be there afterwards.
    let mut caps = cap::empty_caps().expect("dynamic table check caps");
    let mut last = 0;
    for r in &refs {
        let cap = cap::Cap::Endpoint { obj: *r, send: true, recv: false };
        let Some(handle) = cap::install(&mut caps, cap) else {
            let _ = writeln!(console, "dynamic tables: capability FAILED — table did not grow");
            return;
        };
        last = handle as usize;
    }
    let caps_ok = last == N && caps.get(N).is_some_and(Option::is_some);

    // Notifications. These have no destroy path yet, so they stay allocated; at
    // a few bytes each that is a fair price for knowing the table is real.
    let notifs = (0..N).filter(|_| notify::create().is_some()).count();

    // Hand the object slots back. Revoking frees them for reuse, so the demo's
    // own objects land in these slots instead of the table climbing forever —
    // which also exercises the reuse path before anything depends on it.
    for r in &refs {
        obj::revoke(*r);
    }

    let ok = objects_ok && caps_ok && notifs == N;
    let _ = writeln!(
        console,
        "dynamic tables: {N} objects (old max 8), {N} caps in one task (old max 4), \
         {notifs} notifications (old max 4) — {}",
        if ok { "all grew past the old fixed limits" } else { "GREW SHORT" },
    );
}

/// Handle a fault taken from EL0. Bound to the arch trap by link name, like
/// `kmain` — a user task that faults must not take the kernel down with it.
///
/// Two outcomes. A data abort just below the task's stack is not an error at all
/// but the *mechanism* by which a stack grows: the kernel maps a page and returns
/// `true`, and the faulting instruction is retried. Anything else terminates the
/// task and schedules another, in which case this never returns.
#[no_mangle]
pub extern "Rust" fn staros_user_fault(far: u64, esr: u64) -> bool {
    let ec = (esr >> 26) & 0x3f;
    // Data abort from a lower EL. Only a *translation* fault (DFSC 0b0001xx) can be
    // stack growth: a permission or alignment abort on a mapped page is a real bug,
    // and growing the stack for it would hide it.
    const EC_DATA_ABORT_LOWER_EL: u64 = 0x24;
    let dfsc = esr & 0x3f;
    let translation_fault = (0b000100..=0b000111).contains(&dfsc);
    if ec == EC_DATA_ABORT_LOWER_EL && translation_fault && sched::grow_stack_current(far) {
        return true;
    }
    // Name the guard region specially. A fault just below `USER_STACK_LIMIT` is not
    // a wild pointer but a stack that ran past the limit — the exact case the guard
    // exists to catch, and worth distinguishing in a log where every other kill
    // looks the same.
    let guard = (addrspace::USER_STACK_LIMIT.saturating_sub(1024 * 1024)
        ..addrspace::USER_STACK_LIMIT)
        .contains(&far);
    let what = if guard { " — stack guard: growth limit reached" } else { "" };
    // Through the console lock, like every other line. This used to build its own
    // `Pl011` and write straight to the UART, on a comment that said "single-core"
    // — which stopped being true at 1.8 and left one path that could interleave.
    // It cost nothing until a task on another core printed a long line at the wrong
    // moment, and then a fault report was spliced through the middle of it.
    klog!(
        "[fault] task {} killed: EL0 fault at {far:#x} (ec {ec:#04x}){what} — isolated, \
         kernel continues",
        sched::current_id(),
    );
    // Tear the task down exactly as `Exit` would and switch to the next runnable
    // one. The faulting instruction is never retried.
    sched::exit()
}

/// Kernel-side entry for a user task. The scheduler has already installed this
/// task's `TTBR0`, so its EL0 pages are live; we simply drop to EL0 at the shared
/// user entry point on the private user stack. Control returns to the kernel only
/// via a syscall — `Exit` ends the task, so this never returns here.
/// Kernel-side entry for a *thread*: like [`user_task_entry`], but it enters EL0
/// at the address its creator named, on the stack its creator allocated, with the
/// argument its creator passed. A thread with no `user_start` recorded cannot
/// exist — `spawn_thread` always sets one — so a missing one is a kernel bug and
/// ends the task rather than guessing an entry point.
extern "C" fn user_thread_entry() {
    let Some((entry, sp, arg)) = sched::current_user_start() else {
        klog!("[thread] started with no entry recorded; killing it");
        sched::exit()
    };
    // SAFETY: the creator's address space is active (this thread shares it), the
    // entry lies in its EL0-executable image and the stack in EL0-writable
    // anonymous memory it just mapped.
    unsafe { usermode::enter_el0(entry, sp, arg) };
}

extern "C" fn user_task_entry() {
    // The predecessor this core switched away from was already settled and reaped by
    // the entry trampoline (`staros_post_switch`) before we got here, so there is
    // nothing to reclaim at this point — we go straight to dropping into EL0.
    // The program's entry point comes from its ELF `e_entry`, recorded in this
    // task's address space by the loader.
    let entry = sched::current_user_entry();
    // SAFETY: this task's address space is active (its user code/stack pages are
    // mapped EL0-accessible) and the EL1 vectors service its syscalls.
    unsafe { usermode::enter_el0(entry, addrspace::USER_STACK_TOP, 0) };
}

/// Map every `PT_LOAD` segment of the parsed `init` ELF into `space`, each at its
/// own VA with the rights its `p_flags` request. Returns `false` if the ELF is
/// malformed or a segment cannot be mapped (frame pool exhausted or too large for
/// the EL0 window).
///
/// # Safety
/// As [`AddressSpace::map_segment`]: the RAM frame pool must be identity-mapped
/// and writable so each freshly allocated frame is writable here at VA == PA.
unsafe fn load_segments<A: FrameAllocator>(
    space: &mut AddressSpace,
    frames: &mut A,
    image: &elf::Elf,
) -> bool {
    let mut ok = true;
    let walked = image.for_each_load(|seg| {
        // SAFETY: preconditions forwarded from this function's contract.
        if !unsafe { space.map_segment(frames, seg.vaddr, seg.file, seg.memsz, seg.flags) } {
            ok = false;
        }
    });
    walked && ok
}

/// Panic handler. With `panic = "abort"` there is no unwinding; we report to the
/// console (if we can) and stop the core.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Take the screen back if a display server had it. Politeness about someone
    // else's window is over: on a board whose only output is the panel, a panic
    // that stays on the UART is a panic nobody sees.
    console::reclaim_framebuffer();
    klog!("\n*** KERNEL PANIC ***");
    if let Some(loc) = info.location() {
        klog!("at {}:{}", loc.file(), loc.line());
    }
    halt();
}
