//! Notifications — the kernel's asynchronous signalling primitive.
//!
//! An [endpoint](crate::ipc) is a *synchronous* rendezvous that carries a
//! message; a notification is its asynchronous counterpart, carrying only a
//! signal. It is exactly what an interrupt needs: the kernel [`signal`]s a
//! notification from IRQ context, and a user-space driver blocks on it with the
//! `Wait` syscall. Because a signal may arrive before the driver is waiting, each
//! notification keeps a small pending count so no edge is lost.
//!
//! Single-waiter by design (one driver owns one line). A signal delivered while a
//! driver is blocked wakes it and asks for a reschedule; a signal delivered while
//! it is running is remembered in `pending` and consumed by the driver's next
//! `Wait`.


use alloc::vec::Vec;

use crate::sched;
use crate::sync::SpinLock;

/// One notification: whether the slot is in use, how many signals are pending and
/// unconsumed, and the single task (if any) currently blocked in `Wait` on it.
#[derive(Clone, Copy)]
struct Notification {
    used: bool,
    pending: u32,
    waiter: Option<usize>,
}

/// The notification table.
///
/// Everything below takes this lock for a decision and drops it before touching
/// the scheduler. That is not tidiness: it keeps the kernel's lock order a
/// straight line. If `signal` held this while taking the scheduler's lock, and
/// anything on the scheduler's side ever wanted a notification, the two would
/// wait on each other.
/// It grows on demand: one notification per interrupt line a driver registers
/// for, and the number of lines is the machine's business, not a constant's.
static NOTIFY: SpinLock<Vec<Notification>> = SpinLock::new(Vec::new());

/// Allocate a notification, returning its table id, or `None` if the heap is
/// exhausted.
pub fn create() -> Option<usize> {
    let mut slots = NOTIFY.lock();
    if let Some(i) = slots.iter().position(|s| !s.used) {
        slots[i] = Notification { used: true, pending: 0, waiter: None };
        return Some(i);
    }
    // Reachable from the `IrqRegister` syscall, so growth must be fallible: a
    // driver asking for one line too many gets an error, not a dead kernel.
    slots.try_reserve(1).ok()?;
    slots.push(Notification { used: true, pending: 0, waiter: None });
    Some(slots.len() - 1)
}

/// Signal notification `id`: wake its blocked waiter (and ask for a reschedule so
/// the driver runs promptly), or, if none is waiting, remember the signal in the
/// pending count. Safe to call from IRQ context — it only flips scheduler task
/// state, never switches.
pub fn signal(id: usize) {
    let waiter = {
        let mut slots = NOTIFY.lock();
        let Some(slot) = slots.get_mut(id) else {
            return;
        };
        if !slot.used {
            return;
        }
        match slot.waiter.take() {
            Some(task) => Some(task),
            None => {
                slot.pending = slot.pending.saturating_add(1);
                None
            }
        }
    };
    // Outside the lock: waking a task reaches into the scheduler.
    if let Some(task) = waiter {
        sched::unblock(task);
        sched::request_resched();
    }
}

/// Block the current task until notification `id` is signalled, consuming one
/// pending signal. Returns immediately if one is already pending. `id` must name
/// a live notification (the syscall layer resolves it from a capability first).
pub fn wait(id: usize) {
    // Ask who we are *before* taking the lock: `current_id` reaches into the
    // scheduler, and this table's lock must never be held while doing that.
    let me = sched::current_id();
    {
        let mut slots = NOTIFY.lock();
        let Some(slot) = slots.get_mut(id) else {
            return;
        };
        if slot.pending > 0 {
            slot.pending -= 1;
            return;
        }
        // Record ourselves as the waiter *before* releasing the lock and
        // blocking. The lock masks interrupts, so no signal can slip in between.
        slot.waiter = Some(me);
    }
    // Park until `signal` wakes us. Resumes here once the scheduler reschedules us.
    sched::block_current();
}
