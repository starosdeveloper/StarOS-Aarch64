//! ARM Generic Interrupt Controller — GICv2 and GICv3.
//!
//! Both are arch-side implementations of the portable
//! [`staros_hal::InterruptController`] trait, and which one the machine has is
//! discovered at *runtime* from the device tree rather than chosen at build
//! time. The kernel constructs a [`GicSpec`] from what it found and calls
//! [`init`]; everything above the HAL trait — `kernel::irq`, the notification
//! bindings, the user-space driver protocol — is unchanged either way. That is
//! the abstraction earning its keep: two quite different controllers, one
//! interrupt subsystem.
//!
//! The two designs differ more than the shared trait suggests:
//!
//! - **GICv2** has a *distributor* (GICD, routing) and a memory-mapped per-CPU
//!   *interface* (GICC, acknowledge/EOI). Everything is MMIO.
//! - **GICv3** replaces the CPU interface with **system registers**
//!   (`ICC_*_EL1`) — acknowledging an interrupt is now an `mrs`, not a load —
//!   and splits the distributor's per-CPU state into a *redistributor* (GICR)
//!   frame per core, which must be woken out of low-power before it will deliver
//!   anything. SGIs and PPIs (id < 32) are configured in the redistributor;
//!   SPIs (id >= 32) in the distributor, and each must additionally be *routed*
//!   to a specific core.
//!
//! Every Pixel is GICv3. QEMU `virt` can be either, which is what makes this
//! testable today: `-M virt,gic-version=2` and `gic-version=3` boot the same
//! image.

use core::arch::asm;
use core::cell::UnsafeCell;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicUsize, Ordering};

use staros_abi::error::{KError, KResult};
use staros_hal::{AcknowledgingController, InterruptController};

/// Distributor MMIO base on QEMU `virt`. Only a fallback for when there is no
/// device tree to ask; a real machine reports its own.
pub const GICD_BASE: usize = 0x0800_0000;
/// CPU interface MMIO base on QEMU `virt` (GICv2 only).
pub const GICC_BASE: usize = 0x0801_0000;

// ---------------------------------------------------------------------------
// Distributor registers, shared by v2 and v3 (offsets from the GICD base).
// ---------------------------------------------------------------------------
const GICD_CTLR: usize = 0x000;
const GICD_TYPER: usize = 0x004;
const GICD_IGROUPR: usize = 0x080; // one bit per interrupt
const GICD_ISENABLER: usize = 0x100; // one bit per interrupt
const GICD_ICENABLER: usize = 0x180; // one bit per interrupt
const GICD_IPRIORITYR: usize = 0x400; // one byte per interrupt
const GICD_ITARGETSR: usize = 0x800; // v2 only: one byte per interrupt, a CPU-interface mask
const GICD_IROUTER: usize = 0x6000; // v3 only: 64 bits per SPI, from id 32
const GICD_SGIR: usize = 0xF00; // v2 only: write to generate a software interrupt (SGI)

// CPU-interface registers (offsets from `GICC_BASE`), GICv2 only.
const GICC_CTLR: usize = 0x000;
const GICC_PMR: usize = 0x004; // priority mask
const GICC_IAR: usize = 0x00C; // interrupt acknowledge
const GICC_EOIR: usize = 0x010; // end of interrupt

// ---------------------------------------------------------------------------
// Redistributor registers, GICv3 only. Each core owns one frame, itself split
// into an RD_base page and an SGI_base page 64 KiB above it.
// ---------------------------------------------------------------------------
const GICR_CTLR: usize = 0x0000;
const GICR_TYPER: usize = 0x0008; // 64-bit
const GICR_WAKER: usize = 0x0014;
/// Offset of the SGI/PPI page within a redistributor frame.
const GICR_SGI_OFFSET: usize = 0x1_0000;
const GICR_IGROUPR0: usize = 0x0080; // within the SGI page
const GICR_ISENABLER0: usize = 0x0100;
const GICR_ICENABLER0: usize = 0x0180;
const GICR_IPRIORITYR: usize = 0x0400;

/// `GICR_WAKER.ProcessorSleep` — set while the redistributor is asleep.
const GICR_WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
/// `GICR_WAKER.ChildrenAsleep` — clears once the wake-up has taken effect.
const GICR_WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
/// `GICR_TYPER.Last` — this is the final redistributor frame.
const GICR_TYPER_LAST: u64 = 1 << 4;
/// `GICR_TYPER.VLPIS` — the implementation has virtual LPI frames, which
/// doubles the stride between consecutive redistributors.
const GICR_TYPER_VLPIS: u64 = 1 << 1;
/// Distance between redistributor frames without VLPI support (RD + SGI pages).
const GICR_STRIDE: usize = 0x2_0000;
/// Distance between redistributor frames with VLPI support (+ VLPI/reserved).
const GICR_STRIDE_VLPI: usize = 0x4_0000;

/// `GICD_CTLR.ARE_NS` — affinity routing, which GICv3 requires.
const GICD_CTLR_ARE_NS: u32 = 1 << 4;
/// `GICD_CTLR.EnableGrp1A`.
const GICD_CTLR_ENABLE_G1A: u32 = 1 << 1;
/// `GICD_CTLR.EnableGrp1`.
const GICD_CTLR_ENABLE_G1: u32 = 1 << 0;
/// `GICD_CTLR.RWP` / `GICR_CTLR.RWP` — a register write is still in progress.
const RWP: u32 = 1 << 31;

/// The first SPI id. Ids below this are per-core (SGIs 0..16, PPIs 16..32) and
/// on GICv3 live in the redistributor rather than the distributor.
const SPI_BASE: u32 = 32;
/// One above the largest routable interrupt id.
const INTID_LIMIT: u32 = 1020;

/// The value an acknowledge returns when there is no pending interrupt.
const SPURIOUS_INTID: u32 = 1023;
/// Interrupt id field of `GICC_IAR` (GICv2: 10 bits).
const INTID_MASK_V2: u32 = 0x3FF;
/// Interrupt id field of `ICC_IAR1_EL1` (GICv3: 24 bits).
const INTID_MASK_V3: u32 = 0x00FF_FFFF;

/// Highest priority value, i.e. "let everything through" when used as a mask.
const PRIORITY_UNMASKED: u32 = 0xFF;

/// Which controller the machine has, and where it lives.
///
/// The kernel builds this from the device tree — `arm,gic-v3` gives two `reg`
/// ranges (distributor, redistributors), `arm,cortex-a15-gic` gives distributor
/// and CPU interface — so the arch layer never has to guess. Every base here is
/// **physical**, exactly as the tree reports it; [`init`] translates them.
#[derive(Clone, Copy, Debug)]
pub enum GicSpec {
    /// A GICv2 at the given distributor and CPU-interface bases.
    V2 {
        /// Distributor MMIO base.
        gicd: usize,
        /// CPU interface MMIO base.
        gicc: usize,
    },
    /// A GICv3 at the given distributor and redistributor-array bases.
    V3 {
        /// Distributor MMIO base.
        gicd: usize,
        /// Base of the redistributor frames (one per core, contiguous).
        gicr: usize,
    },
}

impl GicSpec {
    /// A short name for the architecture, for boot messages.
    #[must_use]
    pub fn version(&self) -> &'static str {
        match self {
            Self::V2 { .. } => "GICv2",
            Self::V3 { .. } => "GICv3",
        }
    }
}

/// The system's interrupt controller, whichever kind it turned out to be.
pub enum Gic {
    /// [`init`] has not run yet: every operation fails rather than touching
    /// MMIO that may not exist.
    Unconfigured,
    /// A GICv2.
    V2(Gicv2),
    /// A GICv3.
    V3(Gicv3),
}

/// Wrapper making the controller a `static`. All access is volatile MMIO or
/// system registers, so no interior mutability is needed beyond installing the
/// driver once at boot.
struct GicSlot(UnsafeCell<Gic>);

// SAFETY: the slot is written exactly once by `init` during single-core early
// boot, before interrupts are unmasked, and is read-only thereafter. The
// drivers themselves hold only base addresses and touch hardware through
// volatile accesses.
unsafe impl Sync for GicSlot {}

static CONTROLLER: GicSlot = GicSlot(UnsafeCell::new(Gic::Unconfigured));

/// Install and bring up the interrupt controller the machine reported.
///
/// This is the one place the controller's **physical** bases — which is all the
/// device tree deals in — become addresses the kernel can dereference. Every
/// driver below therefore holds linear-map addresses and never thinks about
/// translation again.
///
/// # Safety
/// Call once, at EL1, during early boot with interrupts masked, and only with a
/// `spec` whose base addresses are the real *physical* MMIO windows of a
/// controller of that architecture — they are written to.
///
/// # Errors
/// [`KError::InvalidArgument`] if a GICv3's redistributor for this core cannot
/// be found, which means the `spec` does not describe this machine.
pub unsafe fn init(spec: GicSpec) -> KResult<()> {
    let gic = match spec {
        GicSpec::V2 { gicd, gicc } => Gic::V2(Gicv2 {
            gicd: virt(gicd),
            gicc: virt(gicc),
        }),
        GicSpec::V3 { gicd, gicr } => Gic::V3(Gicv3 {
            gicd: virt(gicd),
            gicr_base: virt(gicr),
            rd: [const { AtomicUsize::new(0) }; crate::boot::MAX_CPUS],
        }),
    };
    // SAFETY: single-core early boot, before any interrupt can be taken; this is
    // the only writer of the slot.
    unsafe { *CONTROLLER.0.get() = gic };

    match controller() {
        // SAFETY: forwarded from this function's contract.
        Gic::V2(d) => unsafe { d.init_global() },
        // SAFETY: as above.
        Gic::V3(d) => unsafe { d.init_global() },
        Gic::Unconfigured => unreachable!("just installed"),
    }
    // The primary is a core like any other and needs its own CPU interface.
    // SAFETY: we are that core, at EL1, with interrupts still masked.
    unsafe { init_cpu() }
}

/// Bring *this* core's part of the controller online.
///
/// Every core has interrupt hardware of its own — a banked CPU interface on
/// GICv2, a redistributor plus `ICC_*_EL1` on GICv3 — and none of it is set up by
/// the core that ran [`init`]. A secondary that skips this takes no interrupts,
/// which shows up as "the timer never preempts anything on core 1".
///
/// # Safety
/// Call once per core, at EL1, after [`init`] and before that core unmasks
/// interrupts.
///
/// # Errors
/// [`KError::NotSupported`] if [`init`] has not run, or
/// [`KError::InvalidArgument`] if this core's GICv3 redistributor cannot be
/// found — which means the `spec` does not describe this machine.
pub unsafe fn init_cpu() -> KResult<()> {
    match controller() {
        Gic::Unconfigured => Err(KError::NotSupported),
        // SAFETY: forwarded from this function's contract.
        Gic::V2(d) => {
            // SAFETY: forwarded from this function's contract.
            unsafe { d.init_cpu() };
            Ok(())
        }
        Gic::V3(d) => {
            // Find *our* redistributor before touching it: the scan matches on
            // this core's affinity, so each core lands on its own frame.
            // SAFETY: `gicr_base` is the redistributor array per `init`'s contract.
            let rd = unsafe { find_redistributor(d.gicr_base) }.ok_or(KError::InvalidArgument)?;
            d.rd[crate::boot::cpu_id() as usize].store(rd, Ordering::Relaxed);
            // SAFETY: our frame is recorded, so `rd()` now answers for this core.
            unsafe { d.init_cpu() };
            Ok(())
        }
    }
}

/// Turn a device tree interrupt specifier into a GIC interrupt id.
///
/// The ARM GIC binding says `<kind, number, flags>`, where kind 0 means an SPI —
/// a shared peripheral line, numbered from [`SPI_BASE`] — and kind 1 a PPI, a
/// line private to each core, numbered from 16. Anything else (kind 2 is the
/// extended ranges) this kernel does not route.
///
/// This mapping is the reason a kernel can stop hard-coding interrupt numbers:
/// the tree says "the UART is SPI 1" and that becomes 33 here, on any machine.
#[must_use]
pub fn intid_from_dt(kind: u32, number: u32) -> Option<u32> {
    let id = match kind {
        0 => SPI_BASE.checked_add(number)?,
        1 => 16u32.checked_add(number)?,
        _ => return None,
    };
    (id < INTID_LIMIT).then_some(id)
}

/// Where a physical MMIO base is reached from the kernel.
fn virt(phys: usize) -> usize {
    crate::mmu::phys_to_virt(phys as u64) as usize
}

/// The installed controller. Before [`init`] every operation fails safely.
#[must_use]
pub fn controller() -> &'static Gic {
    // SAFETY: the slot is only mutated by `init` during early boot; afterwards
    // this is a shared read of an immutable value.
    unsafe { &*CONTROLLER.0.get() }
}

impl InterruptController for Gic {
    fn enable(&self, irq: u32) -> KResult<()> {
        match self {
            Self::Unconfigured => Err(KError::InvalidArgument),
            Self::V2(g) => g.enable(irq),
            Self::V3(g) => g.enable(irq),
        }
    }

    fn disable(&self, irq: u32) -> KResult<()> {
        match self {
            Self::Unconfigured => Err(KError::InvalidArgument),
            Self::V2(g) => g.disable(irq),
            Self::V3(g) => g.disable(irq),
        }
    }

    fn end_of_interrupt(&self, irq: u32) {
        match self {
            Self::Unconfigured => {}
            Self::V2(g) => g.end_of_interrupt(irq),
            Self::V3(g) => g.end_of_interrupt(irq),
        }
    }
}

impl AcknowledgingController for Gic {
    fn acknowledge(&self) -> Option<u32> {
        match self {
            Self::Unconfigured => None,
            Self::V2(g) => g.acknowledge(),
            Self::V3(g) => g.acknowledge(),
        }
    }
}

impl Gic {
    /// Send inter-processor SGI `intid` (0..16) to every core *except* this one.
    ///
    /// This is the kernel's only cross-core doorbell: a core that has just made
    /// work runnable rings it so an idle core stops waiting and re-checks the run
    /// queue, instead of sleeping until its next timer tick. A no-op before the
    /// controller is configured, and — by construction of the "all but self"
    /// routing — when this is the only core online.
    ///
    /// Both back-ends issue a `dsb ish` first so the memory the wake is *about*
    /// (the newly-`Ready` task, published under the scheduler lock) is visible to
    /// the woken core before the SGI arrives.
    pub fn send_sgi_all_but_self(&self, intid: u32) {
        match self {
            Self::Unconfigured => {}
            Self::V2(g) => g.send_sgi_all_but_self(intid),
            Self::V3(g) => g.send_sgi_all_but_self(intid),
        }
    }
}

/// Read this core's affinity as GICv3 encodes it: `Aff3.Aff2.Aff1.Aff0` packed
/// into 32 bits. `MPIDR_EL1` scatters `Aff3` up at bit 32, so it cannot simply
/// be truncated.
fn mpidr_affinity() -> u32 {
    let mpidr: u64;
    // SAFETY: reading MPIDR_EL1 is permitted at EL1 and has no side effect.
    unsafe {
        asm!("mrs {m}, mpidr_el1", m = out(reg) mpidr, options(nomem, nostack, preserves_flags));
    }
    let aff0 = mpidr & 0xFF;
    let aff1 = (mpidr >> 8) & 0xFF;
    let aff2 = (mpidr >> 16) & 0xFF;
    let aff3 = (mpidr >> 32) & 0xFF;
    ((aff3 << 24) | (aff2 << 16) | (aff1 << 8) | aff0) as u32
}

/// Find the redistributor frame belonging to *this* core.
///
/// The frames are a contiguous array in no guaranteed order, each tagged with
/// the affinity of the core it serves, and the last one says so. Matching by
/// affinity rather than assuming frame 0 is what will keep this correct when
/// secondary cores come up — each will run this and find its own.
///
/// # Safety
/// `gicr` must be the *linear-map* address of a real GICv3 redistributor array
/// (see [`init`], which is where the translation happens).
unsafe fn find_redistributor(gicr: usize) -> Option<usize> {
    let want = mpidr_affinity();
    let mut frame = gicr;
    loop {
        // SAFETY: `GICR_TYPER` is a readable 64-bit register at every frame of a
        // real redistributor array; the loop stops at the frame marked Last.
        let typer = unsafe { read_volatile((frame + GICR_TYPER) as *const u64) };
        if (typer >> 32) as u32 == want {
            return Some(frame);
        }
        if typer & GICR_TYPER_LAST != 0 {
            return None;
        }
        frame += if typer & GICR_TYPER_VLPIS != 0 {
            GICR_STRIDE_VLPI
        } else {
            GICR_STRIDE
        };
    }
}

// ---------------------------------------------------------------------------
// GICv2
// ---------------------------------------------------------------------------

/// A GICv2 driver bound to a distributor and CPU-interface base address.
pub struct Gicv2 {
    gicd: usize,
    gicc: usize,
}

impl Gicv2 {
    #[inline]
    fn gicd_write(&self, off: usize, val: u32) {
        // SAFETY: `off` is a valid, writable GICD register offset.
        unsafe { write_volatile((self.gicd + off) as *mut u32, val) }
    }

    #[inline]
    fn gicc_read(&self, off: usize) -> u32 {
        // SAFETY: `off` is a valid GICC register offset for the CPU interface.
        unsafe { read_volatile((self.gicc + off) as *const u32) }
    }

    #[inline]
    fn gicc_write(&self, off: usize, val: u32) {
        // SAFETY: as `gicc_read`, for a writable GICC register.
        unsafe { write_volatile((self.gicc + off) as *mut u32, val) }
    }

    /// The mask identifying *this core's* CPU interface, as `GICD_ITARGETSR`
    /// wants it.
    ///
    /// Read from the hardware rather than derived from our own cpu id, because
    /// the two are different numbering schemes and nothing promises they agree.
    /// `ITARGETSR0` covers interrupts 0-3, which are SGIs: banked, read-only, and
    /// specified to read back as the mask of the interface doing the reading.
    /// That makes this register the GIC telling us who we are — the same trick
    /// Linux's `gic_get_cpumask` uses.
    fn cpu_interface_mask(&self) -> u8 {
        // SAFETY: `ITARGETSR0` is a valid, readable GICD register.
        let targets = unsafe { read_volatile((self.gicd + GICD_ITARGETSR) as *const u32) };
        // Any of the four bytes will do; they all describe this interface.
        let mask = (targets | (targets >> 8) | (targets >> 16) | (targets >> 24)) as u8;
        // A zero would mean "target nobody" and silently drop every SPI. If the
        // hardware will not say who we are, cpu interface 0 is the only interface
        // guaranteed to exist.
        if mask == 0 { 1 } else { mask }
    }

    /// Bring the distributor online. Machine-wide: exactly once, on any core.
    ///
    /// # Safety
    /// Must run once at EL1 during init, before interrupts are unmasked.
    unsafe fn init_global(&self) {
        self.gicd_write(GICD_CTLR, 1);
    }

    /// Generate SGI `intid` on every CPU interface except this one (`GICD_SGIR`
    /// with TargetListFilter = 0b01, "all but the requesting PE").
    fn send_sgi_all_but_self(&self, intid: u32) {
        // Order the writes this wake is about before the doorbell.
        // SAFETY: a barrier is always valid at EL1.
        unsafe { asm!("dsb ish", options(nostack, preserves_flags)) };
        self.gicd_write(GICD_SGIR, (0b01 << 24) | (intid & 0xF));
    }

    /// Bring *this core's* CPU interface online and open its priority mask.
    ///
    /// The GICC registers live at one address but the hardware behind them is
    /// banked per core, so this must run on every core that wants interrupts —
    /// including the timer that preempts it.
    ///
    /// # Safety
    /// Must run once per core at EL1, before that core unmasks interrupts.
    unsafe fn init_cpu(&self) {
        self.gicc_write(GICC_PMR, PRIORITY_UNMASKED);
        self.gicc_write(GICC_CTLR, 1);
    }
}

impl InterruptController for Gicv2 {
    fn enable(&self, irq: u32) -> KResult<()> {
        if irq >= INTID_LIMIT {
            return Err(KError::InvalidArgument);
        }
        // Give the interrupt the highest priority (0) so it passes the mask.
        let prio_off = GICD_IPRIORITYR + irq as usize;
        // SAFETY: byte-wide priority register for a valid interrupt id.
        unsafe { write_volatile((self.gicd + prio_off) as *mut u8, 0) };
        // Route it to a CPU interface. Enabling an SPI says it may fire; it does
        // not say *who* hears it, and on a multi-core GICv2 `ITARGETSR` resets to
        // zero — an enabled interrupt targeting nobody, delivered to no core,
        // ever. Missing this looks like working code on a single-core machine,
        // where the register is specified to read-as-zero/write-ignore and the
        // one CPU interface gets everything regardless.
        //
        // Only SPIs need it: for interrupts below 32 (the SGIs and PPIs, which
        // include our timer) `ITARGETSR` is read-only and banked, already private
        // to each core.
        if irq >= SPI_BASE {
            let target_off = GICD_ITARGETSR + irq as usize;
            let mask = self.cpu_interface_mask();
            // SAFETY: byte-wide target register for a valid SPI id.
            unsafe { write_volatile((self.gicd + target_off) as *mut u8, mask) };
        }
        // Set the enable bit in the appropriate ISENABLER word.
        let reg = GICD_ISENABLER + (irq as usize / 32) * 4;
        self.gicd_write(reg, 1 << (irq % 32));
        Ok(())
    }

    fn disable(&self, irq: u32) -> KResult<()> {
        if irq >= INTID_LIMIT {
            return Err(KError::InvalidArgument);
        }
        // Writing a 1 to ICENABLER clears the enable.
        let reg = GICD_ICENABLER + (irq as usize / 32) * 4;
        self.gicd_write(reg, 1 << (irq % 32));
        Ok(())
    }

    fn end_of_interrupt(&self, irq: u32) {
        self.gicc_write(GICC_EOIR, irq & INTID_MASK_V2);
    }
}

impl AcknowledgingController for Gicv2 {
    fn acknowledge(&self) -> Option<u32> {
        let intid = self.gicc_read(GICC_IAR) & INTID_MASK_V2;
        if intid == SPURIOUS_INTID {
            None
        } else {
            Some(intid)
        }
    }
}

// ---------------------------------------------------------------------------
// GICv3
// ---------------------------------------------------------------------------

/// A GICv3 driver bound to a distributor and *this core's* redistributor frame.
///
/// The CPU interface is not a field here because it is not memory: it is the
/// `ICC_*_EL1` system registers, which are per-core by construction.
pub struct Gicv3 {
    gicd: usize,
    /// Base of the redistributor array, so each core can find its own frame.
    gicr_base: usize,
    /// Each core's redistributor frame (RD_base), indexed by cpu id.
    ///
    /// **Not one frame.** A redistributor is per-core hardware, and SGIs/PPIs —
    /// the timer among them — are configured *there* rather than in the
    /// distributor. A single shared `rd` would mean a secondary enabling its own
    /// timer actually enabled the primary's, which is a bug that looks exactly
    /// like "the timer just does not fire on core 1".
    rd: [AtomicUsize; crate::boot::MAX_CPUS],
}

impl Gicv3 {
    #[inline]
    fn gicd_read(&self, off: usize) -> u32 {
        // SAFETY: `off` is a valid GICD register offset.
        unsafe { read_volatile((self.gicd + off) as *const u32) }
    }

    #[inline]
    fn gicd_write(&self, off: usize, val: u32) {
        // SAFETY: `off` is a valid, writable GICD register offset.
        unsafe { write_volatile((self.gicd + off) as *mut u32, val) }
    }

    /// This core's redistributor frame, as recorded by its own `init_cpu`.
    #[inline]
    fn rd(&self) -> usize {
        self.rd[crate::boot::cpu_id() as usize].load(Ordering::Relaxed)
    }

    /// Write a register in this core's SGI/PPI redistributor page.
    #[inline]
    fn sgi_write(&self, off: usize, val: u32) {
        // SAFETY: `off` is a valid, writable register in the SGI page of a real
        // redistributor frame.
        unsafe { write_volatile((self.rd() + GICR_SGI_OFFSET + off) as *mut u32, val) }
    }

    /// Wait for a distributor register write to take effect.
    ///
    /// Enable/disable on a GICv3 is *asynchronous*: the write is posted and
    /// `RWP` reports when it has actually landed. Returning early would let the
    /// caller believe a line is masked while an interrupt is still on its way —
    /// precisely the storm that `kernel::irq` masks the line to prevent.
    fn wait_rwp_gicd(&self) {
        while self.gicd_read(GICD_CTLR) & RWP != 0 {
            core::hint::spin_loop();
        }
    }

    /// Wait for a redistributor register write to take effect.
    fn wait_rwp_gicr(&self) {
        // SAFETY: `GICR_CTLR` is readable at the RD page of a real frame.
        while unsafe { read_volatile((self.rd() + GICR_CTLR) as *const u32) } & RWP != 0 {
            core::hint::spin_loop();
        }
    }

    /// Largest interrupt id the distributor implements.
    fn max_intid(&self) -> u32 {
        // GICD_TYPER.ITLinesNumber: the distributor supports 32*(N+1) lines.
        let lines = self.gicd_read(GICD_TYPER) & 0x1F;
        ((lines + 1) * 32).min(INTID_LIMIT)
    }

    /// Bring the distributor online. Machine-wide: exactly once, on any core.
    ///
    /// # Safety
    /// Must run once at EL1 during init, before interrupts are unmasked, with
    /// `gicd` pointing at a real GICv3.
    unsafe fn init_global(&self) {
        // --- Distributor -------------------------------------------------
        // Quiesce it before reconfiguring, then wait for that to land.
        self.gicd_write(GICD_CTLR, 0);
        self.wait_rwp_gicd();

        // Put every SPI in Group 1: Group 0 interrupts are FIQs and would be
        // taken to EL3 firmware, not to us.
        let max = self.max_intid();
        for id in (SPI_BASE..max).step_by(32) {
            self.gicd_write(GICD_IGROUPR + (id as usize / 8), 0xFFFF_FFFF);
        }

        // Affinity routing is not optional on v3 — without ARE the IROUTER
        // registers used by `enable` have no effect.
        self.gicd_write(
            GICD_CTLR,
            GICD_CTLR_ARE_NS | GICD_CTLR_ENABLE_G1A | GICD_CTLR_ENABLE_G1,
        );
        self.wait_rwp_gicd();
    }

    /// Bring *this core's* redistributor and system-register CPU interface
    /// online.
    ///
    /// Both are per-core hardware, so every core runs this for itself — a
    /// secondary that skipped it would take no interrupts at all, and one that
    /// ran it against another core's redistributor frame would silently arm the
    /// wrong core's timer.
    ///
    /// # Safety
    /// Must run once per core at EL1, before that core unmasks interrupts, with
    /// this core's redistributor already recorded by [`init_cpu`](Gic::init_cpu).
    unsafe fn init_cpu(&self) {
        // --- Redistributor (this core) -----------------------------------
        // It comes out of reset asleep and will deliver nothing until woken.
        // SAFETY: `GICR_WAKER` is a readable/writable register at the RD page.
        unsafe {
            let waker = (self.rd() + GICR_WAKER) as *mut u32;
            let val = read_volatile(waker) & !GICR_WAKER_PROCESSOR_SLEEP;
            write_volatile(waker, val);
            // The wake-up is asynchronous; children stay asleep until it lands.
            while read_volatile(waker) & GICR_WAKER_CHILDREN_ASLEEP != 0 {
                core::hint::spin_loop();
            }
        }

        // SGIs and PPIs — the timer among them — are configured here, not in
        // the distributor. Group 1, same reasoning as the SPIs above.
        self.sgi_write(GICR_IGROUPR0, 0xFFFF_FFFF);
        self.wait_rwp_gicr();

        // --- CPU interface (system registers) ----------------------------
        // SAFETY: these are the EL1 GIC system registers. `ICC_SRE_EL1.SRE`
        // must be enabled (and take effect, hence the `isb`) before any other
        // `ICC_*` access is architecturally valid.
        unsafe {
            asm!(
                "mrs {tmp}, icc_sre_el1",
                "orr {tmp}, {tmp}, #1",
                "msr icc_sre_el1, {tmp}",
                "isb",
                tmp = out(reg) _,
                options(nostack, preserves_flags),
            );
            // Priority mask wide open, no priority grouping, Group 1 enabled.
            asm!(
                "msr icc_pmr_el1, {pmr}",
                "msr icc_bpr1_el1, xzr",
                "msr icc_igrpen1_el1, {en}",
                "isb",
                pmr = in(reg) u64::from(PRIORITY_UNMASKED),
                en = in(reg) 1u64,
                options(nostack, preserves_flags),
            );
        }
    }

    /// Generate SGI `intid` on every core except this one via `ICC_SGI1R_EL1`
    /// with IRM = 1 ("interrupt routing mode: all PEs but the requester").
    fn send_sgi_all_but_self(&self, intid: u32) {
        // IRM at bit 40, SGI id in [27:24]; the affinity/target-list fields are
        // ignored when IRM = 1.
        let val: u64 = (1u64 << 40) | ((u64::from(intid) & 0xF) << 24);
        // SAFETY: writing ICC_SGI1R_EL1 is permitted at EL1 once SRE is enabled
        // (done in `init_cpu`); the `dsb` orders the wake's memory before it, the
        // `isb` ensures the SGI is generated before we return.
        unsafe {
            asm!(
                "dsb ish",
                "msr icc_sgi1r_el1, {v}",
                "isb",
                v = in(reg) val,
                options(nostack, preserves_flags),
            );
        }
    }
}

impl InterruptController for Gicv3 {
    fn enable(&self, irq: u32) -> KResult<()> {
        if irq >= INTID_LIMIT {
            return Err(KError::InvalidArgument);
        }
        if irq < SPI_BASE {
            // SGI/PPI: private to this core, so it lives in the redistributor.
            // SAFETY: byte-wide priority register for a valid id in the SGI page.
            unsafe {
                write_volatile(
                    (self.rd() + GICR_SGI_OFFSET + GICR_IPRIORITYR + irq as usize) as *mut u8,
                    0,
                );
            }
            self.sgi_write(GICR_ISENABLER0, 1 << irq);
            self.wait_rwp_gicr();
        } else {
            // SPI: shared, so the distributor must be told *which* core to send
            // it to. IRM = 0 with our affinity means "this PE, specifically".
            // SAFETY: 64-bit router register for a valid SPI id, and ARE is on.
            unsafe {
                write_volatile(
                    (self.gicd + GICD_IROUTER + irq as usize * 8) as *mut u64,
                    u64::from(mpidr_affinity()),
                );
                write_volatile((self.gicd + GICD_IPRIORITYR + irq as usize) as *mut u8, 0);
            }
            self.gicd_write(GICD_ISENABLER + (irq as usize / 32) * 4, 1 << (irq % 32));
            self.wait_rwp_gicd();
        }
        Ok(())
    }

    fn disable(&self, irq: u32) -> KResult<()> {
        if irq >= INTID_LIMIT {
            return Err(KError::InvalidArgument);
        }
        if irq < SPI_BASE {
            self.sgi_write(GICR_ICENABLER0, 1 << irq);
            self.wait_rwp_gicr();
        } else {
            self.gicd_write(GICD_ICENABLER + (irq as usize / 32) * 4, 1 << (irq % 32));
            self.wait_rwp_gicd();
        }
        Ok(())
    }

    fn end_of_interrupt(&self, irq: u32) {
        // SAFETY: writing ICC_EOIR1_EL1 with a previously acknowledged id drops
        // the running priority and deactivates it (EOImode = 0).
        unsafe {
            asm!(
                "msr icc_eoir1_el1, {v}",
                v = in(reg) u64::from(irq & INTID_MASK_V3),
                options(nostack, preserves_flags),
            );
        }
    }
}

impl AcknowledgingController for Gicv3 {
    fn acknowledge(&self) -> Option<u32> {
        let iar: u64;
        // SAFETY: reading ICC_IAR1_EL1 acknowledges the highest-priority pending
        // Group 1 interrupt; permitted at EL1 once SRE is enabled.
        unsafe {
            asm!("mrs {v}, icc_iar1_el1", v = out(reg) iar, options(nostack, preserves_flags));
        }
        let intid = iar as u32 & INTID_MASK_V3;
        // 1020..=1023 are reserved; 1023 in particular means "nothing pending".
        if intid >= INTID_LIMIT {
            None
        } else {
            Some(intid)
        }
    }
}
