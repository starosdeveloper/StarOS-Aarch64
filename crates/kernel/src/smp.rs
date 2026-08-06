//! Bringing the other cores up.
//!
//! A phone has eight cores and hands the kernel exactly one. The rest are held
//! in reset by firmware, and the only way to ask for them is PSCI `CPU_ON` —
//! which needs two things the kernel must get from two different places: *which*
//! cores exist (the device tree's `/cpus`, by MPIDR) and *how* to ask (the
//! device tree's PSCI conduit, see [`staros_arch_aarch64::psci`]).
//!
//! Bring-up is deliberately **serialised**: the primary starts one core, waits
//! for it to say it is alive, and only then starts the next. It costs a few
//! microseconds once, and it buys a boot log that is readable and a failure that
//! names the core that failed instead of a machine that half-started.

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use staros_arch_aarch64::gic;
use staros_arch_aarch64::uart::Pl011;
use staros_arch_aarch64::timer::{self, GenericTimer};
use staros_arch_aarch64::{boot, exceptions, psci};
use staros_fdt::Fdt;
use staros_hal::InterruptController;

use crate::console::klog;
use crate::sched;
use crate::sync::SpinLock;

/// Bitmask of cores that have reached [`ksecondary`], bit `n` = cpu `n`.
///
/// An atomic because this is the one piece of state written by a core that the
/// kernel has no lock protecting yet — it is what the primary polls to decide
/// whether the core it just asked for actually arrived.
static ONLINE: AtomicU64 = AtomicU64::new(0);

/// How long to wait for a core to come up before giving up on it, in polls.
///
/// Firmware that accepts `CPU_ON` and then does nothing is a real failure mode on
/// real hardware, and one the kernel must survive: a core that never arrives must
/// not hang the boot.
const ONLINE_POLL_LIMIT: u32 = 100_000_000;

/// Mark this core online. Called by each secondary once it can execute Rust.
pub fn mark_online(cpu: u64) {
    ONLINE.fetch_or(1 << cpu, Ordering::Release);
}

/// Whether cpu `cpu` has reported in.
fn is_online(cpu: u64) -> bool {
    ONLINE.load(Ordering::Acquire) & (1 << cpu) != 0
}

/// The number of cores currently running kernel code, including the primary.
#[must_use]
pub fn online_count() -> u32 {
    ONLINE.load(Ordering::Acquire).count_ones() + 1
}

/// The MPIDR values of the machine's cores, in device tree order.
///
/// `/cpus` overrides the root's cell counts — addresses are one cell there, not
/// two — which is exactly the sort of thing that makes hand-rolled DTB parsing
/// wrong on the first real machine. The `reg` iterator honours the node's own
/// `#address-cells`, so this is the tree's answer rather than ours.
fn cpu_mpidrs(fdt: &Fdt<'_>, out: &mut [u64; boot::MAX_CPUS]) -> usize {
    let Some(cpus) = fdt.find_node("/cpus") else {
        return 0;
    };
    let mut n = 0;
    for child in cpus.children() {
        if child.base_name() != "cpu" || n >= boot::MAX_CPUS {
            continue;
        }
        if let Some((mpidr, _)) = child.reg().and_then(|mut r| r.next()) {
            out[n] = mpidr;
            n += 1;
        }
    }
    n
}

/// Start every core the device tree lists except the one we are already on.
///
/// Returns the number of cores running kernel code afterwards, the primary
/// included — so `1` means we are still alone, whatever the tree claimed.
pub fn start_secondaries(console: &mut Pl011, fdt: &Fdt<'_>) -> u32 {
    let mut mpidrs = [0u64; boot::MAX_CPUS];
    let count = cpu_mpidrs(fdt, &mut mpidrs);
    let boot_cpu = fdt.boot_cpuid() as usize;

    if count <= 1 {
        return 1;
    }
    if !psci::is_available() {
        // Worth saying plainly: without PSCI these cores are not merely unused,
        // they are unreachable. A spin table is the other way in, and this kernel
        // does not implement one.
        let _ = writeln!(
            console,
            "smp: {count} cores in the device tree, but no PSCI — cannot start any of them"
        );
        return 1;
    }

    let entry = boot::secondary_entry();
    for (cpu, &mpidr) in mpidrs.iter().enumerate().take(count) {
        if cpu == boot_cpu {
            continue;
        }
        // SAFETY: this core is not running — firmware holds it until the `cpu_on`
        // below — and `cpu` is in range by construction.
        let Some(context) = (unsafe { boot::arm_secondary(cpu) }) else {
            let _ = writeln!(console, "smp: cpu {cpu} is past MAX_CPUS; not started");
            continue;
        };
        if let Err(e) = psci::cpu_on(mpidr, entry, context) {
            let _ = writeln!(console, "smp: cpu {cpu} (mpidr {mpidr:#x}) refused by firmware: {e:?}");
            continue;
        }
        // Wait for it to actually arrive. Firmware saying "yes" is not the same
        // as a core running our code.
        let mut spins = 0u32;
        while !is_online(cpu as u64) && spins < ONLINE_POLL_LIMIT {
            core::hint::spin_loop();
            spins += 1;
        }
        if !is_online(cpu as u64) {
            let _ = writeln!(
                console,
                "smp: cpu {cpu} (mpidr {mpidr:#x}) accepted CPU_ON but never arrived"
            );
        }
    }

    online_count()
}

// ---------------------------------------------------------------------------
// Proving the cores are real
// ---------------------------------------------------------------------------
//
// "4 cores online" only means four cores reached a Rust function and set a bit.
// It does not show they *execute concurrently*, that memory they share is
// coherent, or that the kernel's new lock actually excludes anyone. The test
// below asks all of that at once, and it is arranged so that the wrong answer
// cannot look like the right one.

/// How many times each core adds to the shared counter.
const HAMMER_ITERS: u64 = 20_000;

/// The contended counter. Guarded by a [`SpinLock`], and deliberately a plain
/// `u64` rather than an atomic: an atomic would be correct *without* the lock,
/// which would make this a test of the hardware instead of a test of the lock.
static COUNTER: SpinLock<u64> = SpinLock::new(0);

/// Cores that have finished hammering, so the primary knows when to read the
/// total. Bit `n` = cpu `n`.
static HAMMER_DONE: AtomicU64 = AtomicU64::new(0);

/// Add to the shared counter `HAMMER_ITERS` times, then record that we finished.
fn hammer(cpu: u64) {
    for _ in 0..HAMMER_ITERS {
        let mut n = COUNTER.lock();
        // Read-modify-write through the lock. Non-atomic on purpose: if the lock
        // does not exclude, increments are lost and the total comes out short.
        *n += 1;
    }
    HAMMER_DONE.fetch_or(1 << cpu, Ordering::Release);
}

/// Run the contention test across every online core and report the result.
///
/// Returns `true` if the total was exactly what serialised execution must
/// produce.
pub fn race_test(console: &mut Pl011, cores: u32) -> bool {
    let expected = u64::from(cores) * HAMMER_ITERS;

    // Let the secondaries in. They are spinning on this flag rather than doing
    // the work as they arrive, so that all of the cores contend *at the same
    // time* — a test where core 1 finishes before core 2 starts would pass
    // whether or not the lock works.
    HAMMER_GO.store(true, Ordering::Release);
    hammer(boot::cpu_id());

    // Wait for everyone. The primary is not in the mask (it just ran `hammer`
    // inline), so the bits we want are exactly the secondaries'.
    let want = ONLINE.load(Ordering::Acquire);
    let mut spins = 0u32;
    while HAMMER_DONE.load(Ordering::Acquire) & want != want && spins < ONLINE_POLL_LIMIT {
        core::hint::spin_loop();
        spins += 1;
    }

    let total = *COUNTER.lock();
    let ok = total == expected;
    let _ = writeln!(
        console,
        "smp: {cores} cores x {HAMMER_ITERS} locked increments = {total} (expected {expected}) — {}",
        if ok { "no increments lost" } else { "LOST INCREMENTS" },
    );
    ok
}

/// Released by the primary once every core is up, so they all contend together.
static HAMMER_GO: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Inter-processor interrupts (the wake doorbell)
// ---------------------------------------------------------------------------
//
// `race_test` proves the cores share coherent memory under a lock. It does not
// prove the primary can *signal* another core — the thing an idle core has to be
// woken by, and the basis of any future cross-core call. That needs an SGI, which
// this section adds: one software interrupt id the kernel rings to say "there is
// work; stop waiting and look".

/// The SGI the kernel uses as its cross-core wake doorbell. SGIs are ids 0..16;
/// 0 is fine — nothing else in this kernel uses it.
pub const SGI_WAKE: u32 = 0;

/// Wake SGIs taken, per core. Written from IRQ context by the core that took the
/// SGI, so the falsifiable IPI test can confirm every core actually received one.
static IPI_COUNT: [AtomicU64; boot::MAX_CPUS] = [const { AtomicU64::new(0) }; boot::MAX_CPUS];

/// Enable reception of the wake SGI on the calling core. Each core calls this once
/// its GIC CPU interface is up, so the doorbell can actually reach it.
pub fn enable_wake_ipi() {
    let _ = gic::controller().enable(SGI_WAKE);
}

/// Record that this core took a wake SGI. Called from the IRQ dispatcher.
pub fn on_ipi() {
    IPI_COUNT[boot::cpu_id() as usize].fetch_add(1, Ordering::Relaxed);
}

/// Ring the cross-core doorbell: wake every *other* core so an idle one re-checks
/// the run queue now rather than at its next timer tick. Skipped when we are the
/// only core online, so the single-core path pays nothing.
pub fn wake_others() {
    if online_count() > 1 {
        gic::controller().send_sgi_all_but_self(SGI_WAKE);
    }
}

/// Prove the primary can actively signal the other cores, not just share memory
/// with them: broadcast a wake SGI and confirm every secondary's handler ran.
///
/// This is the exact mechanism [`wake_others`] uses, tested in isolation before
/// any task exists. It rings in a bounded retry loop because a secondary may still
/// be bringing its GIC up when the first ring goes out; once it is up and
/// idle-waiting (IRQs enabled around its `wfi`), a later ring is delivered.
/// Returns `true` only if every secondary reported in.
pub fn ipi_test(console: &mut Pl011, cores: u32) -> bool {
    if cores <= 1 {
        let _ = writeln!(console, "smp: single core — no inter-processor interrupt to send");
        return true;
    }
    let want = ONLINE.load(Ordering::Acquire);
    let secondary_reported = |c: usize| want & (1 << c) != 0 && IPI_COUNT[c].load(Ordering::Acquire) > 0;

    let mut spins = 0u32;
    let reached = loop {
        gic::controller().send_sgi_all_but_self(SGI_WAKE);
        let all = (0..boot::MAX_CPUS).all(|c| want & (1 << c) == 0 || secondary_reported(c));
        if all {
            break true;
        }
        if spins >= ONLINE_POLL_LIMIT {
            break false;
        }
        spins += 1;
        core::hint::spin_loop();
    };

    let got = (0..boot::MAX_CPUS).filter(|&c| secondary_reported(c)).count();
    let _ = writeln!(
        console,
        "smp: wake IPI reached {got}/{} secondaries via SGI {SGI_WAKE} — {}",
        cores - 1,
        if reached { "every core signalled" } else { "SOME CORES MISSED" },
    );
    reached
}

/// Kernel entry for a secondary core, reached from `arch::boot::rust_secondary_start`.
///
/// By the time this runs the core is already where the primary was after its own
/// stub finished: EL1, translation on through the *same* tables, running at its
/// link addresses, on a stack of its own.
///
/// The `#[no_mangle]` name matches the `extern "Rust" { fn ksecondary(cpu) }`
/// declaration in the arch crate — the same link-time seam `kmain` uses.
#[no_mangle]
pub extern "Rust" fn ksecondary(cpu: u64) -> ! {
    mark_online(cpu);

    // Wait for the primary to finish bringing everyone up, then all cores hit the
    // counter together.
    while !HAMMER_GO.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    hammer(cpu);

    // Everything below is per-core hardware that the primary could not set up on
    // our behalf, and each piece has its own way of failing quietly if skipped.

    // `VBAR_EL1` is a *per-core* register, and nothing has set ours. Without this
    // the first `svc` from an EL0 task scheduled here would vector into whatever
    // the register happened to reset to — so this must happen before this core
    // can be given a user task, not merely before it takes an interrupt.
    // SAFETY: at EL1 on this core, before it runs anything that can trap.
    unsafe { exceptions::init() };

    // Our own GIC interface: a banked CPU interface on GICv2, a redistributor
    // plus `ICC_*_EL1` on GICv3. Skipped, this core simply never sees an
    // interrupt.
    // SAFETY: at EL1 after the primary's `gic::init`, with IRQs still masked.
    if unsafe { gic::init_cpu() }.is_err() {
        klog!("smp: cpu {cpu} could not bring up its GIC interface; running without preemption");
        sched::run_secondary();
    }

    // The timer is per-core too, and its interrupt is a PPI — a *private* line,
    // enabled in this core's own redistributor. Enabling it on the primary did
    // nothing for us.
    if gic::controller().enable(timer::intid()).is_err() {
        klog!("smp: cpu {cpu} could not route its timer; running without preemption");
        sched::run_secondary();
    }
    // SAFETY: our timer line is routed; arm the tick that will preempt whatever
    // this core is running.
    unsafe { GenericTimer::arm(GenericTimer::frequency_hz() / crate::TICK_HZ) };

    // Open this core's wake doorbell, so a peer making work runnable can pull it
    // out of `wfi` immediately instead of leaving it asleep until the next tick.
    enable_wake_ipi();

    // Join the scheduler and start taking real work, preemptively.
    sched::run_secondary()
}
