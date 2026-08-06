//! IRQ dispatch — the kernel side of the GIC trap.
//!
//! The arch layer takes the interrupt from the GIC and calls us with its id; we
//! decide what it *means*. The generic timer tick drives preemptive scheduling;
//! every *other* registered line is **forwarded to user space** — the kernel
//! masks it, signals the notification a driver registered for it, and lets that
//! EL0 driver do the actual device work. That is the last piece of "drivers live
//! outside the kernel": not only their MMIO but their interrupts are handled at
//! EL0.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use staros_arch_aarch64::boot;
use staros_arch_aarch64::gic::controller;
use staros_arch_aarch64::timer::{self, GenericTimer};
use staros_hal::InterruptController;

use crate::{notify, sched, smp};
use crate::console::klog;
use crate::sync::SpinLock;

/// A registered interrupt→notification binding.
struct Binding {
    intid: u32,
    notif: usize,
}

/// The interrupt→notification table.
///
/// This was an `UnsafeCell` whose safety argument was, in full, "single-core;
/// every access runs IRQ-masked". Masking is what keeps a handler from racing the
/// core it interrupted, and that was a complete argument while one core existed.
/// It stopped being one when the kernel started the others: a core in `register`
/// (from a syscall) and a core in `lookup` (from its own IRQ) mask only
/// *themselves*, so nothing prevented one from rewriting the table while the
/// other read it. The lock is what the masking was standing in for.
///
/// Taking it from IRQ context is safe precisely because it masks: a core cannot
/// be interrupted into wanting a lock it already holds.
static BINDINGS: SpinLock<Vec<Binding>> = SpinLock::new(Vec::new());

/// Bind interrupt `intid` to notification `notif` and enable the line at the
/// controller. Returns `false` if the line is already bound or the heap is
/// exhausted. Called from the `IrqRegister` syscall.
pub fn register(intid: u32, notif: usize) -> bool {
    {
        let mut table = BINDINGS.lock();
        if table.iter().any(|b| b.intid == intid) {
            return false;
        }
        if table.try_reserve(1).is_err() {
            return false;
        }
        table.push(Binding { intid, notif });
    }
    // Route it, with the lock dropped: the driver, not the kernel, will service
    // the device from now on.
    controller().enable(intid).is_ok()
}

/// Re-enable interrupt `intid` at the controller after a driver has serviced it.
/// The forwarding path masks the line on each fire (so a level-triggered source
/// cannot storm); this undoes that mask. Called from the `IrqAck` syscall.
pub fn ack(intid: u32) {
    let _ = controller().enable(intid);
}

/// Look up the notification bound to `intid`, if any.
fn lookup(intid: u32) -> Option<usize> {
    let table = BINDINGS.lock();
    table.iter().find(|b| b.intid == intid).map(|b| b.notif)
}

/// Number of timer ticks observed since boot.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Timer ticks taken *per core*.
///
/// The total alone cannot tell you whether the other cores are being preempted:
/// a secondary that never set up its GIC interface simply contributes nothing and
/// the number still climbs. Per-core, a zero is an answer.
static TICKS_PER_CPU: [AtomicU64; boot::MAX_CPUS] = [const { AtomicU64::new(0) }; boot::MAX_CPUS];

/// Timer reload interval, in counter ticks. Set once at setup so the IRQ handler
/// can re-arm without recomputing it. `0` until [`set_tick_interval`] runs.
static TICK_INTERVAL: AtomicU64 = AtomicU64::new(0);

/// Record the reload interval (in counter ticks) used to re-arm the timer on
/// each tick. Call once during timer setup.
pub fn set_tick_interval(ticks: u64) {
    TICK_INTERVAL.store(ticks, Ordering::Relaxed);
}

/// Ticks counted since boot.
#[must_use]
pub fn tick_count() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Timer ticks taken by core `cpu` — i.e. how many times its own timer actually
/// preempted it. Zero on a core whose interrupt hardware never came up.
#[must_use]
pub fn tick_count_for(cpu: usize) -> u64 {
    TICKS_PER_CPU[cpu].load(Ordering::Relaxed)
}

/// Kernel IRQ dispatcher, invoked from the arch GIC trap with the acknowledged
/// interrupt id.
#[no_mangle]
pub extern "Rust" fn staros_irq_dispatch(intid: u32) {
    // Ids 0..16 are SGIs — here, only the cross-core wake doorbell. Count it (the
    // IPI self-test reads these) and request a reschedule so a busy core picks up
    // whatever a peer just made runnable; an idle core re-checks its run queue on
    // its own when `wfi` returns. No device work, no EOI beyond the arch layer's.
    if intid < 16 {
        smp::on_ipi();
        sched::request_resched();
        return;
    }

    if intid == timer::intid() {
        TICKS.fetch_add(1, Ordering::Relaxed);
        TICKS_PER_CPU[boot::cpu_id() as usize].fetch_add(1, Ordering::Relaxed);

        // Re-arm for the next tick (the timer is one-shot per fire).
        // SAFETY: at EL1 servicing the timer IRQ; reprogramming CNTP is exactly
        // what acknowledging the timer requires.
        unsafe { GenericTimer::arm(TICK_INTERVAL.load(Ordering::Relaxed)) };

        // Ask for a reschedule; the actual switch happens in the epilogue, after
        // this interrupt has been EOI'd.
        sched::request_resched();
        return;
    }

    // A user-space driver registered for this line: mask it at the controller so
    // a level-triggered source (e.g. a non-empty UART RX FIFO) cannot re-fire
    // before the driver has serviced it, then signal the driver's notification.
    // The driver re-enables the line with `IrqAck` once it has drained the device.
    if let Some(notif) = lookup(intid) {
        let _ = controller().disable(intid);
        notify::signal(notif);
        return;
    }

    klog!("[irq] spurious/unknown intid={intid}");
}

/// Called by the arch IRQ handler after EOI. Performs the deferred reschedule so
/// task switching happens with no interrupt active on the GIC.
#[no_mangle]
pub extern "Rust" fn staros_irq_epilogue() {
    sched::on_irq_epilogue();
}
