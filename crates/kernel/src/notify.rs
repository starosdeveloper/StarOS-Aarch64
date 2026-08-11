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
//!
//! One waiter per notification does **not** mean one notification per waiter:
//! [`wait_any`] lets a task stand in several queues at once, which is what an
//! event loop needs and what `poll` does on a POSIX system. The extra rule that
//! buys is deregistration — see that function's comment on the signal a departing
//! waiter would otherwise swallow.


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

/// Signal notification `id`: count the signal, and wake its blocked waiter (asking
/// for a reschedule so the driver runs promptly) if one is parked. Safe to call
/// from IRQ context — it only flips scheduler task state, never switches.
///
/// **The count is incremented whether or not anyone is waiting**, and that is not
/// how this started. It used to hand the signal *straight* to a parked waiter and
/// increment `pending` only when nobody was there — which works for `Wait`, where
/// being woken is itself the answer, and breaks [`wait_any`], where the woken task
/// then has to ask *which* source fired. It could only find out by looking at the
/// counts, and the count it needed had never been incremented: the task woke,
/// found nothing pending, and went back to sleep until its deadline.
///
/// The bug was invisible on one core — there, the server always signalled before
/// the client reached its wait, so the signal went through the `pending` path — and
/// showed up on the four-core configs of the smoke matrix, where the client really
/// does park first. It is the exact race the roadmap predicted for this phase, and
/// it took the matrix rather than a local run to see it.
pub fn signal(id: usize) {
    let waiter = {
        let mut slots = NOTIFY.lock();
        let Some(slot) = slots.get_mut(id) else {
            return;
        };
        if !slot.used {
            return;
        }
        slot.pending = slot.pending.saturating_add(1);
        slot.waiter.take()
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
    // The one-source case of [`wait_any`], written as one, so there is a single
    // implementation of the parking rules rather than two that can drift.
    let _ = wait_any(&[id], None);
}

/// Block until **any** of `ids` is signalled, consuming one signal from whichever
/// fired, and return its index in `ids`. Returns `None` if `deadline_ns` passes
/// first (or immediately, if it has already passed).
///
/// This is the primitive an event loop is built on, and the reason `Wait` alone is
/// not enough: `poll` waits on a *set* with a timeout and reports *which* member
/// is ready. The index is returned rather than the id because the caller indexes
/// its own array with it, and because an index cannot be confused with an error
/// code the way a table id could.
///
/// ## The two races this has to survive
///
/// **A signal that arrives while we are on our way to sleep.** We register as the
/// waiter on every id under this table's lock, drop it, and only then take the
/// scheduler's lock to park. A signal in that window finds us still `Running`, so
/// it cannot mark us `Ready` — instead `sched::unblock` records `wake_pending`,
/// which the parking path consumes and declines to block. Without it we would
/// sleep holding a signal nobody will send again.
///
/// **A signal that arrives after we have woken, on a source we no longer watch.**
/// This is the one that costs a *lost signal* rather than a lost wake-up: while we
/// are still registered, `signal` hands the wake to us instead of incrementing
/// `pending` — and if we are no longer waiting, that signal simply evaporates. So
/// waking deregisters from **every** id, not just the one that fired, before doing
/// anything else. The loop below re-checks after deregistering, which also covers
/// the case where a peer consumed the signal we were woken for.
pub fn wait_any(ids: &[usize], deadline_ns: Option<u64>) -> Option<usize> {
    // Ask who we are *before* taking the lock: `current_id` reaches into the
    // scheduler, and this table's lock must never be held while doing that.
    let me = sched::current_id();
    loop {
        {
            let mut slots = NOTIFY.lock();
            if let Some(index) = consume_ready(&mut slots, ids) {
                return Some(index);
            }
            // Nothing ready. If the caller cannot wait, say so now rather than
            // parking a task that asked not to be parked.
            if let Some(deadline) = deadline_ns {
                if sched::clock_now() >= deadline {
                    return None;
                }
            }
            // Register on every source *before* releasing the lock. The lock masks
            // interrupts, so no signal can slip in between the check above and this.
            for &id in ids {
                if let Some(slot) = slots.get_mut(id) {
                    if slot.used {
                        slot.waiter = Some(me);
                    }
                }
            }
        }

        // Park until a signal wakes us, or the deadline does.
        sched::block_until(deadline_ns);

        // Awake. Deregister from everything first — see the doc comment: a source
        // we still appear to watch swallows its next signal instead of counting it.
        {
            let mut slots = NOTIFY.lock();
            for &id in ids {
                if let Some(slot) = slots.get_mut(id) {
                    if slot.waiter == Some(me) {
                        slot.waiter = None;
                    }
                }
            }
            if let Some(index) = consume_ready(&mut slots, ids) {
                return Some(index);
            }
        }

        // Woken with nothing to show for it: either the deadline passed, or a peer
        // consumed the signal first. The first ends the wait; the second goes round
        // again, which is why this is a loop and not a straight line.
        if let Some(deadline) = deadline_ns {
            if sched::clock_now() >= deadline {
                return None;
            }
        }
    }
}

/// Consume one pending signal from the first ready id, returning its index.
fn consume_ready(slots: &mut [Notification], ids: &[usize]) -> Option<usize> {
    for (index, &id) in ids.iter().enumerate() {
        if let Some(slot) = slots.get_mut(id) {
            if slot.used && slot.pending > 0 {
                slot.pending -= 1;
                return Some(index);
            }
        }
    }
    None
}
