//! Round-robin scheduler for kernel threads and EL0 user tasks.
//!
//! This is the kernel-side *policy* built on the arch context-switch primitive.
//! Tasks are cooperative threads that also get preempted by the timer tick (via
//! [`preempt`], called from the IRQ epilogue). Each task carries a `TTBR0` value:
//! a kernel thread runs in the base identity map, while a user task ([`spawn_user`])
//! runs in its own address space. The switch loads the next task's `TTBR0` before
//! entering it, so distinct EL0 processes are isolated from one another. A user
//! task's entry function drops to EL0 (see the kernel's `user_task_entry`); when
//! it is preempted at EL0 the full user state lives in the trap frame on its
//! kernel stack, which the callee-saved context switch preserves.
//!
//! ## Locking discipline
//! The scheduler lives in a single [`SpinLock`]. Crucially the guard is never
//! held across a [`context_switch`]: doing so would both alias the scheduler from
//! the task we switch to *and* leave the lock owned by a task that is no longer
//! running, which every other core would then wait on forever. Instead each
//! operation takes the lock briefly, extracts raw `CpuContext` pointers, drops
//! the guard, and only then switches.
//!
//! Interrupts are a separate concern from the lock and outlive it: masking must
//! span the whole switch, or the timer could preempt this core between choosing a
//! task and entering it. So the callers mask around the entire operation and the
//! lock's own masking simply nests inside that.
//!
//! ## What makes this safe on several cores
//! `pick_next` only ever returns a `Ready` slot, and the picker marks it
//! `Running` before dropping the guard. So two cores cannot select the same task:
//! by the time the second one looks, the slot is no longer `Ready`. `current` and
//! the bootstrap context are per-core — a core must not be able to ask "which task
//! am I running" and get another core's answer.
//!
//! That handles two cores *selecting* the same task. It does **not** by itself
//! handle a core selecting a task another core is still *switching away from*:
//! marking a task runnable (under the lock) and saving its context (in the
//! `context_switch` after the lock is dropped) are not atomic together, so a slot
//! can read "runnable" with a stale saved `ctx`. Loading that stale context
//! resumes the task on a superseded stack — the classic intermittent multi-core
//! corruption. The [`Task::on_cpu`] flag closes it: a task stays "on cpu" from the
//! moment it is marked `Running` until its *successor* clears the flag after the
//! switch has saved its context, and [`Scheduler::pickable`] refuses to hand out a
//! task while the flag is set. So a task being switched away from is simply skipped
//! until its context is safely saved — no stale load, and no switch-path spin that
//! could deadlock in a cycle.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use staros_abi::error::KError;
use staros_arch_aarch64::addrspace::AddressSpace;
use staros_arch_aarch64::context::{context_switch, CpuContext};
use staros_arch_aarch64::{boot, exceptions, mmu};

use crate::cap::{self, Cap, CapTable};
use crate::ipc::KMessage;
use crate::sync::SpinLock;

/// Per-task kernel stack size in 64-bit words (32 KiB).
const STACK_WORDS: usize = 4096;

/// Lifecycle state of a task slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Runnable but not currently on the CPU.
    Ready,
    /// Currently executing.
    Running,
    /// Waiting for an event (e.g. an IPC message). Not schedulable until an
    /// [`unblock`] makes it `Ready` again.
    Blocked,
    /// Finished; slot will not be scheduled again.
    Dead,
}

/// A schedulable task: its saved context, its own kernel stack, and the address
/// space (`TTBR0`) to activate before it runs. Kernel threads use the base
/// identity map; user tasks use a private space built in `arch::addrspace`.
#[repr(C, align(16))]
struct Task {
    ctx: CpuContext,
    /// This task's kernel stack, on the heap rather than inline in the slot.
    ///
    /// Not merely to keep [`Task`] small: a task's stack must not move for as
    /// long as the task exists, and an inline array moves whenever the table
    /// holding it does. A separate allocation is pinned by construction.
    stack: Box<[u64]>,
    state: State,
    id: u64,
    /// `TTBR0_EL1` value to install when switching into this task.
    ttbr0: u64,
    /// The user address space this task runs in, if any. `None` for kernel
    /// threads (they run in the base identity map). Used by `MapMemory` to add a
    /// device mapping to the *caller's* space.
    space: Option<AddressSpace>,
    /// This task's capability table: the objects it may act on, named by handle.
    caps: CapTable,
    /// Message delivered to this task while it was blocked in `Recv`, read when
    /// it resumes. `None` unless a sender has just handed it a message.
    mailbox: Option<KMessage>,
    /// A wakeup that arrived *before* this task managed to park.
    ///
    /// This closes a lost-wakeup race: a task decides to block by registering as
    /// a waiter under the IPC lock, drops it, and only then takes the scheduler
    /// lock to set itself `Blocked`. A peer waking it in that window sees it still
    /// `Running`, so it cannot flip it to `Ready` — instead it sets this flag, and
    /// the parking task checks it (under the scheduler lock, the same one the peer
    /// held) and declines to block. Without it, on several cores the task would
    /// sleep forever holding a message nobody will redeliver.
    wake_pending: bool,
    /// `true` while some core is *running this task or still saving its context*.
    ///
    /// This closes a subtler, deadlier SMP race than `wake_pending`. Making a task
    /// runnable again (`reschedule` flips it `Ready`; an `unblock` flips a parked
    /// task `Ready`) happens under the scheduler lock — but the task's *registers
    /// and SP are not saved into its `ctx` until the `context_switch` that follows,
    /// which runs with the lock **dropped**. In that window the slot says "runnable"
    /// while its saved context is stale: a peer core that picks it up loads an
    /// SP/LR from the task's *previous* suspension and runs on a stack the task has
    /// since moved past — a corrupted resume that lands as a wild PC (`ec=0x22`,
    /// jumps to address 2, undefined instructions).
    ///
    /// So `on_cpu` gates *pickability*: [`Scheduler::pickable`] skips a `Ready` task
    /// whose `on_cpu` is still set, and the outgoing task's successor clears it only
    /// *after* the switch has saved the context (see [`post_switch`]). A task caught
    /// in that window is simply passed over and picked on a later scan — no core ever
    /// loads a half-saved context, and there is no cross-core spin to deadlock. Set
    /// when a task is marked `Running`; every access is under the scheduler lock.
    on_cpu: AtomicBool,
}

/// Allocate a zeroed kernel stack, or `None` if the heap is exhausted.
///
/// Fallible on purpose: `Spawn` is a syscall, so "out of memory" is an answer
/// user space is allowed to provoke and must therefore receive as an error rather
/// than as a panicking kernel.
fn alloc_stack() -> Option<Box<[u64]>> {
    let mut stack: Vec<u64> = Vec::new();
    stack.try_reserve_exact(STACK_WORDS).ok()?;
    // Neither of these can reallocate: the capacity is already exact.
    stack.resize(STACK_WORDS, 0);
    Some(stack.into_boxed_slice())
}

/// The initial `SP` for a task standing on `stack`: the top, rounded *down* to a
/// 16-byte boundary.
///
/// AArch64 faults on a stack access made through a misaligned `SP`. While the
/// stack was an array inside a `#[repr(align(16))]` slot that was free; a
/// `Box<[u64]>` only promises 8-byte alignment, so the guarantee has to be
/// re-established here rather than assumed.
fn stack_top(stack: &[u64]) -> u64 {
    let top = stack.as_ptr() as u64 + (stack.len() * 8) as u64;
    top & !0xf
}

/// Move `task` to the heap, or `None` if the heap is exhausted.
///
/// `Box::new` aborts on allocation failure and `Box::try_new` is still unstable,
/// so this goes the long way round through the fallible API that does exist: a
/// `Vec` with exactly one element's worth of capacity cannot reallocate on
/// `push` or on `into_boxed_slice`, and a one-element `Box<[Task]>` has the same
/// layout as a `Box<Task>`.
fn try_box(task: Task) -> Option<Box<Task>> {
    let mut v: Vec<Task> = Vec::new();
    v.try_reserve_exact(1).ok()?;
    v.push(task);
    let boxed: Box<[Task]> = v.into_boxed_slice();
    let ptr = Box::into_raw(boxed).cast::<Task>();
    // SAFETY: `ptr` came from `Box::into_raw` on a one-element slice, so it is a
    // valid, uniquely-owned, correctly-aligned `Task` allocated by the global
    // allocator with `Layout::array::<Task>(1)` — which is `Layout::new::<Task>()`.
    // Re-boxing it as a single element therefore frees it with the layout it was
    // allocated with.
    Some(unsafe { Box::from_raw(ptr) })
}

/// The scheduler state.
struct Scheduler {
    /// Every task, live or dead.
    ///
    /// `Vec<Box<Task>>` rather than `Vec<Task>`, and the indirection is load
    /// bearing. A core suspended inside [`context_switch`] is writing through a
    /// raw pointer into its slot while other cores keep scheduling; if a spawn on
    /// one of those cores grew a `Vec<Task>`, every existing task would be
    /// *memcpy'd to a new address* and that pointer — along with every suspended
    /// task's saved `SP` — would be left pointing into freed memory. Boxing each
    /// task means growth moves the pointers, never the tasks.
    ///
    /// `clippy::vec_box` reads this as redundant indirection, which is the right
    /// call whenever a `Vec`'s elements are only ever reached *through the `Vec`*.
    /// These are not: their addresses escape into `TTBR`-era raw pointers and into
    /// live stack pointers, so the extra indirection is the invariant.
    #[allow(clippy::vec_box)]
    tasks: Vec<Box<Task>>,
    /// Each core's bootstrap context: for the primary, `kmain` as saved by
    /// [`start`]; for a secondary, its idle loop in [`run_secondary`]. A core
    /// returns here when it has nothing left to run.
    bootstrap: [CpuContext; boot::MAX_CPUS],
    /// The `TTBR0` each core was in when it began scheduling, restored when it
    /// returns to its bootstrap context.
    bootstrap_ttbr0: [u64; boot::MAX_CPUS],
    /// The task each core is running. Per-core: "which task am I in" must never
    /// be answerable with another core's task.
    current: [usize; boot::MAX_CPUS],
}

impl Scheduler {
    const fn new() -> Self {
        Self {
            tasks: Vec::new(),
            bootstrap: [const { CpuContext::empty() }; boot::MAX_CPUS],
            bootstrap_ttbr0: [0; boot::MAX_CPUS],
            current: [usize::MAX; boot::MAX_CPUS],
        }
    }

    /// Whether slot `i` may be picked to run *now*: it is `Ready`, and no core is
    /// still saving its context (`on_cpu` clear). Skipping an on-cpu task is what
    /// keeps a peer from loading a context another core has not finished writing —
    /// see [`Task::on_cpu`]. The task becomes pickable on a later scan, the moment
    /// its outgoing core's successor clears the flag.
    fn pickable(&self, i: usize) -> bool {
        self.tasks[i].state == State::Ready && !self.tasks[i].on_cpu.load(Ordering::Relaxed)
    }

    /// First pickable slot, if any.
    fn first_ready(&self) -> Option<usize> {
        (0..self.tasks.len()).find(|&i| self.pickable(i))
    }

    /// Next pickable slot after `from`, scanning round-robin (never returns
    /// `from` itself).
    fn pick_next(&self, from: usize) -> Option<usize> {
        let n = self.tasks.len();
        if n == 0 {
            return None;
        }
        (1..=n)
            .map(|off| (from + off) % n)
            .find(|&i| self.pickable(i))
    }

    /// Whether any task is `Ready` or already `Running` on some core.
    ///
    /// Deliberately *not* counting `Blocked`: a task waiting for an event nobody
    /// will send is not work, and treating it as work is how a kernel hangs
    /// instead of finishing. This matches what the single-core scheduler did when
    /// it stopped as soon as nothing was runnable.
    fn any_runnable(&self) -> bool {
        self.tasks
            .iter()
            .any(|t| matches!(t.state, State::Ready | State::Running))
    }
}

static SCHED: SpinLock<Scheduler> = SpinLock::new(Scheduler::new());

/// Sleep this core until an interrupt arrives, then return so the caller can
/// re-check the run queue. Replaces a busy spin in the idle loops: a core with
/// nothing to run drops to `wfi` instead of burning power.
///
/// IRQs are enabled only across the `wfi`, so the wakeup event — a peer's wake IPI
/// or the periodic timer tick — is actually serviced and returns us; on either
/// side of this the scheduler's masked-critical-section invariant holds. A task
/// becoming ready in the gap between the caller's check and the `wfi` costs at
/// most one timer tick of latency (the tick always fires), never a lost wakeup —
/// and the wake IPI closes even that gap whenever a peer is the one making work.
fn idle_wait() {
    // SAFETY: at EL1 with vectors and the GIC up; IRQs are re-masked immediately,
    // preserving the caller's masked section. `current[cpu]` is `usize::MAX` while
    // idling, so a timer tick taken here reschedules to nothing and returns.
    unsafe {
        exceptions::enable_irqs();
        exceptions::wait_for_interrupt();
        exceptions::disable_irqs();
    }
}

/// Set by the timer tick to request a reschedule at the next IRQ epilogue.
static NEED_RESCHED: AtomicBool = AtomicBool::new(false);

/// Per-core slot index of the task this core last switched *away from*, handed to
/// whichever context this core resumes next so it can, in [`post_switch`], release
/// that task's `on_cpu` (its context is saved now) and — if it exited — free its
/// kernel stack. `usize::MAX` means the switch was *from* a bootstrap context (no
/// task) and there is nothing to settle.
///
/// Written by every switch-out ([`reschedule`], [`park_and_switch`], [`exit`]) on
/// this core and read/cleared solely by whichever context this core resumes next,
/// so it is never shared across cores. Its reaping duty is what closes the leak
/// `spawn_user` documents: a dead task's stack no longer waits until reboot.
static PREV: [AtomicUsize; boot::MAX_CPUS] =
    [const { AtomicUsize::new(usize::MAX) }; boot::MAX_CPUS];

/// How many dead-task stacks have been reaped, and how many 64-bit words that
/// returned to the heap — a falsifiable measure of the reclaim (zero would mean
/// the old until-reboot leak is still there).
static REAPED_STACKS: AtomicU64 = AtomicU64::new(0);
static REAPED_WORDS: AtomicU64 = AtomicU64::new(0);

/// Number of dead-task kernel stacks reaped so far, and the bytes that freed.
#[must_use]
pub fn reaped_stacks() -> (u64, u64) {
    (
        REAPED_STACKS.load(Ordering::Relaxed),
        REAPED_WORDS.load(Ordering::Relaxed) * 8,
    )
}

/// Settle the task this core just switched away from, then — if it exited — reclaim
/// its kernel stack. Runs immediately after every `context_switch` that resumes a
/// *successor*, and (for a brand-new task, which begins at its entry trampoline
/// rather than after a `context_switch`) from that trampoline via
/// [`staros_post_switch`], before the new task runs any of its own code.
///
/// "Settle" means clear the predecessor's [`Task::on_cpu`]: the `context_switch`
/// that brought us here has finished saving that task's registers and SP, so
/// [`Scheduler::pickable`] may now hand it to another core. We clear it under the
/// scheduler lock, so the same lock that a picker takes to read `on_cpu` also
/// publishes the just-saved context to it (lock release → lock acquire).
///
/// If the predecessor exited, its stack is now unused by anyone (this core has left
/// it) so we free the 32 KiB allocation — the bulk of a task's kernel memory. The
/// small `Task` tombstone stays so slot indices and task ids remain stable, and its
/// `space`/`caps` were already released at [`exit`]. The freed `Box` is dropped
/// *after* the scheduler lock, so the heap's allocator lock never nests under it.
fn post_switch() {
    let prev = PREV[me()].swap(usize::MAX, Ordering::Relaxed);
    if prev == usize::MAX {
        return;
    }
    let _freed_stack = {
        let mut sched = SCHED.lock();
        // The switch that resumed us has saved `prev`'s context; release it so a
        // picker may run it. Relaxed is enough: every `on_cpu` access is under this
        // lock, whose acquire/release ordering carries the saved context across.
        sched.tasks[prev].on_cpu.store(false, Ordering::Relaxed);
        // Defensive: only a Dead slot that still owns a stack is reapable, so a
        // stray double-reap can never free the same allocation twice.
        if sched.tasks[prev].state == State::Dead && !sched.tasks[prev].stack.is_empty() {
            let old = core::mem::replace(
                &mut sched.tasks[prev].stack,
                Vec::<u64>::new().into_boxed_slice(),
            );
            REAPED_STACKS.fetch_add(1, Ordering::Relaxed);
            REAPED_WORDS.fetch_add(old.len() as u64, Ordering::Relaxed);
            Some(old)
        } else {
            None
        }
    };
    // `_freed_stack` drops here, outside the scheduler lock.
}

/// Post-switch settle for a *freshly created* task: its entry trampoline calls this
/// (with IRQs still masked, as inherited from the switching-out core) before it
/// enables interrupts and runs the task body, so the predecessor this core switched
/// away from is released and reaped exactly as on any other resume path.
#[no_mangle]
pub extern "Rust" fn staros_post_switch() {
    post_switch();
}

/// This core's index, as the scheduler keys its per-core state.
#[inline]
fn me() -> usize {
    boot::cpu_id() as usize
}

/// Create a user task: a thread whose `entry` (a kernel-side trampoline) drops
/// to EL0, running in `space` with the capability table `caps`. The task
/// remembers its space so `MapMemory` can add device mappings to it, and its
/// caps so every syscall is authority-checked against what it was granted.
///
/// The task table grows to fit, so this fails only when the heap is exhausted —
/// which, unlike the fixed table it replaces, is a limit set by the machine
/// rather than by a number chosen in advance.
///
/// Returns `false` if the task could not be allocated.
pub fn spawn_user(entry: extern "C" fn(), space: AddressSpace, caps: CapTable) -> bool {
    // Build the whole task *before* taking the lock. Partly to keep the lock
    // short, but mainly to keep the lock order a straight line: allocating takes
    // the heap's lock, and doing that underneath the scheduler's would nest two
    // locks for no reason.
    let Some(stack) = alloc_stack() else {
        return false;
    };
    let mut ctx = CpuContext::empty();
    ctx.init(entry, stack_top(&stack));
    let Some(task) = try_box(Task {
        ctx,
        stack,
        state: State::Ready,
        id: 0,
        ttbr0: space.ttbr0(),
        space: Some(space),
        caps,
        mailbox: None,
        wake_pending: false,
        on_cpu: AtomicBool::new(false),
    }) else {
        return false;
    };

    let mut sched = SCHED.lock();
    // Append rather than reuse a `Dead` slot. Reuse would need proof that the dead
    // task has left the stack, and its *slot* is still a live tombstone (task ids
    // and `current[cpu]` index into it). The heavy part — the 32 KiB kernel stack —
    // is reclaimed the moment the dead task's successor runs (`reap_dead`), which is
    // the safe point the comment used to say did not exist: the successor is, by
    // definition, off the dead stack. So a task's stack no longer waits until
    // reboot; only the small `Task` header lingers, bounded by how many tasks ran.
    if sched.tasks.try_reserve(1).is_err() {
        return false;
    }
    let mut task = task;
    task.id = sched.tasks.len() as u64;
    sched.tasks.push(task);
    drop(sched);
    // A task is runnable now — ring the doorbell so an idle core picks it up at
    // once instead of sleeping until its next tick.
    crate::smp::wake_others();
    true
}

/// Begin scheduling. Saves the bootstrap context and switches into the first
/// ready task; returns here only once every task has exited.
pub fn start() {
    // SAFETY: enter an IRQ-masked critical section for the initial switch.
    let saved = unsafe { exceptions::irq_save() };

    let cpu = me();
    let base_ttbr0 = mmu::ttbr0();
    SCHED.lock().bootstrap_ttbr0[cpu] = base_ttbr0;

    // A loop, not a single switch. With other cores scheduling too, coming back
    // here means only "this core had nothing ready at that instant" — another
    // core may still be running a task that will unblock more work. We are done
    // only when nothing is runnable anywhere.
    loop {
        let picked = {
            let mut sched = SCHED.lock();
            sched.first_ready().map(|first| {
                sched.current[cpu] = first;
                sched.tasks[first].state = State::Running;
                // On-cpu until our successor releases it after the switch saves it.
                sched.tasks[first].on_cpu.store(true, Ordering::Relaxed);
                // Switching *from* the bootstrap context, which is not a task —
                // nothing to settle when the successor resumes here.
                PREV[cpu].store(usize::MAX, Ordering::Relaxed);
                let boot: *mut CpuContext = &mut sched.bootstrap[cpu];
                let next: *const CpuContext = &sched.tasks[first].ctx;
                (boot, next, sched.tasks[first].ttbr0)
            })
        };
        match picked {
            Some((boot_ptr, first_ptr, first_ttbr0)) => {
                // Enter the task's address space, then switch into it.
                // SAFETY: a valid root table; the kernel is in TTBR1 and
                // unaffected by the switch.
                unsafe { mmu::set_ttbr0(first_ttbr0) };
                // SAFETY: pointers reference distinct contexts; IRQs are masked.
                // We resume here when this core runs out of tasks.
                unsafe { context_switch(boot_ptr, first_ptr) };
                // Back in the bootstrap context: settle (and, if it exited, reap)
                // the task that switched back to us.
                post_switch();
            }
            None => {
                if !SCHED.lock().any_runnable() {
                    break;
                }
                // Something is running elsewhere; sleep until an interrupt (a wake
                // IPI or the timer tick) rather than spin, then look again.
                idle_wait();
            }
        }
    }

    // Back in the bootstrap thread for good: restore the kernel's base map.
    // SAFETY: `base_ttbr0` is the map `kmain` was running in when it called us.
    unsafe { mmu::set_ttbr0(base_ttbr0) };
    // SAFETY: restore the caller's interrupt state.
    unsafe { exceptions::irq_restore(saved) };
}

/// Scheduling loop for a secondary core: run whatever is ready, forever.
///
/// The primary's [`start`] eventually returns to `kmain`; a secondary has no
/// `kmain` to return to, so its bootstrap context is this loop and it simply
/// keeps looking for work.
pub fn run_secondary() -> ! {
    // SAFETY: this core schedules with IRQs masked around each switch, exactly
    // as the primary does. It never returns, so there is no restore.
    let _ = unsafe { exceptions::irq_save() };
    let cpu = me();
    SCHED.lock().bootstrap_ttbr0[cpu] = mmu::ttbr0();

    loop {
        let picked = {
            let mut sched = SCHED.lock();
            sched.first_ready().map(|first| {
                sched.current[cpu] = first;
                sched.tasks[first].state = State::Running;
                // On-cpu until our successor releases it after the switch saves it.
                sched.tasks[first].on_cpu.store(true, Ordering::Relaxed);
                // Switching *from* the bootstrap context (not a task): nothing to
                // settle when the successor returns here.
                PREV[cpu].store(usize::MAX, Ordering::Relaxed);
                let boot: *mut CpuContext = &mut sched.bootstrap[cpu];
                let next: *const CpuContext = &sched.tasks[first].ctx;
                (boot, next, sched.tasks[first].ttbr0)
            })
        };
        match picked {
            Some((boot_ptr, next_ptr, ttbr0)) => {
                // SAFETY: as in `start` — a valid root table, distinct contexts,
                // IRQs masked. We come back here when this core runs dry.
                unsafe { mmu::set_ttbr0(ttbr0) };
                // SAFETY: as above.
                unsafe { context_switch(boot_ptr, next_ptr) };
                // Back in the bootstrap context: settle (and maybe reap) the task
                // that switched back to us.
                post_switch();
            }
            // Nothing ready: sleep until a wake IPI or the timer tick, then look
            // again — a secondary that busy-spun here pinned a core at 100%.
            None => idle_wait(),
        }
    }
}

/// Voluntarily yield the CPU to the next ready task.
pub fn yield_now() {
    // SAFETY: reschedule inside an IRQ-masked critical section.
    let saved = unsafe { exceptions::irq_save() };
    reschedule();
    // SAFETY: matching restore (runs when this task is switched back in).
    unsafe { exceptions::irq_restore(saved) };
}

/// Preempt the current task from the timer IRQ epilogue. IRQs are already masked
/// by exception entry, so no save/restore is needed here.
pub fn preempt() {
    reschedule();
}

/// Request a reschedule at the next IRQ epilogue (called from the timer tick).
pub fn request_resched() {
    NEED_RESCHED.store(true, Ordering::Relaxed);
}

/// Run at the end of IRQ handling: if a reschedule was requested, do it now
/// (after the interrupt has been EOI'd).
pub fn on_irq_epilogue() {
    if NEED_RESCHED.swap(false, Ordering::Relaxed) {
        preempt();
    }
}

/// Core switch: move from the current task to the next ready one, if any. Must
/// be called with IRQs masked. Returns immediately if nothing else is runnable.
fn reschedule() {
    let prev_ptr: *mut CpuContext;
    let next_ptr: *const CpuContext;
    let next_ttbr0: u64;
    {
        let mut sched = SCHED.lock();
        let cpu = me();
        let prev = sched.current[cpu];
        // Not in a task (this core is in its bootstrap loop): nothing to switch
        // away from.
        if prev == usize::MAX {
            return;
        }
        let Some(next) = sched.pick_next(prev) else {
            return;
        };
        if sched.tasks[prev].state == State::Running {
            sched.tasks[prev].state = State::Ready;
        }
        // Claimed before the guard drops, so no other core can pick it too.
        sched.tasks[next].state = State::Running;
        sched.tasks[next].on_cpu.store(true, Ordering::Relaxed);
        sched.current[cpu] = next;
        // Hand `prev` to our successor: it clears `prev.on_cpu` once the switch
        // below has saved `prev`'s context. Until then `prev` is `Ready` but not
        // `pickable`, so no core loads it stale.
        PREV[cpu].store(prev, Ordering::Relaxed);
        prev_ptr = &mut sched.tasks[prev].ctx;
        next_ptr = &sched.tasks[next].ctx;
        next_ttbr0 = sched.tasks[next].ttbr0;
    }
    // Activate the next task's address space before switching into it.
    // SAFETY: a valid root table sharing the kernel identity map; we execute
    // identity-mapped kernel code across the switch.
    unsafe { mmu::set_ttbr0(next_ttbr0) };
    // SAFETY: distinct contexts, IRQs masked. Execution resumes here when this
    // task is scheduled again.
    unsafe { context_switch(prev_ptr, next_ptr) };
    // Resumed as someone's successor: settle (and maybe reap) our predecessor.
    post_switch();
}

/// Terminate the current task and switch away for good. Never returns.
pub fn exit() -> ! {
    // SAFETY: mask IRQs for the final switch; this task never runs again so the
    // mask is not restored on its behalf.
    let _ = unsafe { exceptions::irq_save() };

    let prev_ptr: *mut CpuContext;
    let next_ptr: *const CpuContext;
    let next_ttbr0: u64;
    let dead_space: Option<AddressSpace>;
    {
        let mut sched = SCHED.lock();
        let cpu = me();
        let prev = sched.current[cpu];
        sched.tasks[prev].state = State::Dead;
        // Hand this slot to whichever context this core resumes next: it will
        // release our `on_cpu` and free our kernel stack once the switch below has
        // left it. We cannot free it here — we are still standing on it.
        PREV[cpu].store(prev, Ordering::Relaxed);
        // Take the exiting task's address space so its frames can be reclaimed;
        // leaving `None` ensures it is never torn down twice.
        dead_space = sched.tasks[prev].space.take();
        prev_ptr = &mut sched.tasks[prev].ctx;
        match sched.pick_next(prev) {
            Some(next) => {
                sched.tasks[next].state = State::Running;
                sched.tasks[next].on_cpu.store(true, Ordering::Relaxed);
                sched.current[cpu] = next;
                next_ttbr0 = sched.tasks[next].ttbr0;
                next_ptr = &sched.tasks[next].ctx;
            }
            // Nothing ready for *this* core: go back to its bootstrap loop,
            // which decides whether the system is finished or another core is
            // still producing work.
            None => {
                sched.current[cpu] = usize::MAX;
                next_ttbr0 = sched.bootstrap_ttbr0[cpu];
                next_ptr = &sched.bootstrap[cpu];
            }
        };
    }
    // Activate the successor's address space *first*, so the dead task's page
    // tables are no longer the active translation source before we reclaim them.
    // SAFETY: a valid root table sharing the kernel identity map.
    unsafe { mmu::set_ttbr0(next_ttbr0) };
    // Return the exited task's frames (page tables, data and stack) to the buddy
    // allocator so a future task can reuse them. The shared code frame is not
    // owned by the space and is intentionally left in place.
    if let Some(space) = dead_space {
        crate::mem::with(|frames| {
            // SAFETY: this space's TTBR0 is no longer active (we switched above);
            // freeing only marks the frames reusable, and every frame came from
            // this allocator via `AddressSpace::new`.
            unsafe { space.destroy(frames) };
        });
    }
    // SAFETY: `prev` is a dead slot used only as a write sink; `next` is a live
    // context. IRQs are masked. The dead task is never resumed.
    unsafe { context_switch(prev_ptr, next_ptr) };
    unreachable!("switched away from an exited task");
}

/// The EL0 entry point (ELF `e_entry`) of the currently running user task. Read
/// by the user trampoline to drop to the program's real start address. Returns 0
/// if the current task has no user address space.
#[must_use]
pub fn current_user_entry() -> u64 {
    let sched = SCHED.lock();
    let cur = sched.current[me()];
    match sched.tasks[cur].space {
        Some(space) => space.entry(),
        None => 0,
    }
}

/// Index of the currently running task. Used by the IPC layer to name the
/// caller when it blocks on an endpoint.
#[must_use]
pub fn current_id() -> usize {
    let sched = SCHED.lock();
    sched.current[me()]
}

/// How many task slots the table holds.
///
/// A high-water mark rather than a live count, since slots are never reused (see
/// [`spawn_user`]). It is the kernel's own answer to "how many tasks have there
/// ever been", which is worth more than counting lines in the log: tasks report
/// themselves a byte at a time through `DebugPutc`, and on several cores two of
/// them interleave mid-line, so a line count undercounts exactly when the table
/// is under the most pressure.
#[must_use]
pub fn task_count() -> usize {
    SCHED.lock().tasks.len()
}

/// How many tasks still hold an address space, i.e. how many still own frames.
///
/// [`start`] returns when nothing is *runnable*, which is not the same as nothing
/// being *alive*: a task blocked in `Wait` still exists and its memory is still
/// legitimately its own. Anyone auditing the frame pool afterwards has to know
/// the difference before calling a shortfall a leak.
#[must_use]
pub fn live_spaces() -> usize {
    let sched = SCHED.lock();
    sched.tasks.iter().filter(|t| t.space.is_some()).count()
}

/// Mark the current task `Blocked` and switch away, resuming here when it is made
/// `Ready` again and next scheduled. The caller must already have arranged for
/// something to wake it (an endpoint wait queue). Runs in an IRQ-masked critical
/// section around the switch.
fn park_and_switch() {
    // SAFETY: on entry we are in a syscall handler with IRQs already masked;
    // save/restore keeps that honest across the switch.
    let saved = unsafe { exceptions::irq_save() };

    let prev_ptr: *mut CpuContext;
    let next_ptr: *const CpuContext;
    let next_ttbr0: u64;
    {
        let mut sched = SCHED.lock();
        let cpu = me();
        let prev = sched.current[cpu];
        // Lost-wakeup guard. Between registering as a waiter (under the IPC lock)
        // and reaching here, a peer may have delivered to us and — seeing us still
        // `Running` — set `wake_pending` rather than `Ready`. Consume it and do not
        // block: the event we were going to wait for has already happened.
        if sched.tasks[prev].wake_pending {
            sched.tasks[prev].wake_pending = false;
            drop(sched);
            // SAFETY: matching restore for the save at the top of this function.
            unsafe { exceptions::irq_restore(saved) };
            return;
        }
        sched.tasks[prev].state = State::Blocked;
        // Hand `prev` to our successor to release once the switch has saved its
        // context. Crucially, an `unblock` may flip `prev` back to `Ready` the
        // instant we drop this lock — before the switch below saves it — but its
        // `on_cpu` stays set until our successor clears it, so `pickable` keeps a
        // peer from loading `prev` before its context is safely saved.
        PREV[cpu].store(prev, Ordering::Relaxed);
        prev_ptr = &mut sched.tasks[prev].ctx;
        match sched.pick_next(prev) {
            Some(next) => {
                sched.tasks[next].state = State::Running;
                sched.tasks[next].on_cpu.store(true, Ordering::Relaxed);
                sched.current[cpu] = next;
                next_ttbr0 = sched.tasks[next].ttbr0;
                next_ptr = &sched.tasks[next].ctx;
            }
            // Nothing ready for this core: fall back to its bootstrap loop
            // rather than spin a task that is, by definition, not runnable.
            None => {
                sched.current[cpu] = usize::MAX;
                next_ttbr0 = sched.bootstrap_ttbr0[cpu];
                next_ptr = &sched.bootstrap[cpu];
            }
        }
    }
    // Activate the successor's address space, then switch. We resume here once a
    // peer unblocks us and the scheduler picks us again.
    // SAFETY: a valid root table sharing the kernel identity map.
    unsafe { mmu::set_ttbr0(next_ttbr0) };
    // SAFETY: distinct contexts, IRQs masked.
    unsafe { context_switch(prev_ptr, next_ptr) };
    // Resumed as someone's successor: settle (and maybe reap) our predecessor.
    post_switch();

    // SAFETY: matching restore for the save above.
    unsafe { exceptions::irq_restore(saved) };
}

/// Block the current task until a message is delivered to it, then return that
/// message. Called from the `Recv` path when no message is buffered.
#[must_use]
pub fn block_for_message() -> KMessage {
    park_and_switch();
    // Resumed: our mailbox holds the message a sender delivered.
    let mut sched = SCHED.lock();
    let me = sched.current[me()];
    sched.tasks[me].mailbox.take().expect("woken without a message")
}

/// Block the current task until a peer unblocks it. Called from the `Send` path
/// when the endpoint ring is full; the receiver that drains it wakes us.
pub fn block_current() {
    park_and_switch();
}

/// Make a blocked task runnable again without switching to it. The caller keeps
/// running; the woken task runs when the scheduler next picks it.
///
/// If the task has not yet parked (still `Running` in the race window), record
/// the wakeup so its imminent `park_and_switch` declines to block — see
/// [`Task::wake_pending`].
pub fn unblock(task: usize) {
    {
        let mut sched = SCHED.lock();
        if sched.tasks[task].state == State::Blocked {
            sched.tasks[task].state = State::Ready;
        } else {
            sched.tasks[task].wake_pending = true;
        }
    }
    // The task may now be runnable on another core — nudge idle cores.
    crate::smp::wake_others();
}

/// Deliver `km` to a blocked receiver and make it runnable. Does not switch. As in
/// [`unblock`], a task caught mid-park gets `wake_pending` set instead of `Ready`.
pub fn deliver(task: usize, km: KMessage) {
    {
        let mut sched = SCHED.lock();
        sched.tasks[task].mailbox = Some(km);
        if sched.tasks[task].state == State::Blocked {
            sched.tasks[task].state = State::Ready;
        } else {
            sched.tasks[task].wake_pending = true;
        }
    }
    // The receiver is runnable now — nudge idle cores to schedule it.
    crate::smp::wake_others();
}

/// Install `cap` into the current task's capability table at the first free slot,
/// returning the new handle (or `None` if the table is full). This is how a
/// received capability becomes usable — dynamic, per-task capability allocation.
pub fn install_cap_current(cap: Cap) -> Option<u32> {
    let mut sched = SCHED.lock();
    let me = sched.current[me()];
    // Growing the table allocates, so this takes the heap's lock under the
    // scheduler's. That order is safe because the heap is a leaf — nothing under
    // it reaches back into the scheduler — and it is the same direction as the
    // scheduler-then-frames nesting in `map_anon_current`. The reverse never
    // happens.
    cap::install(&mut sched.tasks[me].caps, cap)
}

/// Resolve `handle` against the *current* task's capability table, returning the
/// capability it names (a `Copy` value) or `None` if the slot is empty or out of
/// range. Slot 0 is the reserved null handle and never resolves.
#[must_use]
pub fn resolve_cap(handle: u32) -> Option<Cap> {
    if handle == 0 {
        return None;
    }
    let sched = SCHED.lock();
    let cur = sched.current[me()];
    // `get` rather than an index: the table is per-task and sized to what that
    // task was granted, so a handle past its end is an ordinary "no such
    // capability" and must not be a panic.
    sched.tasks[cur].caps.get(handle as usize).copied().flatten()
}

/// Map the device page at physical `dev_phys` into the *current* task's address
/// space and return the resulting user virtual address (or a negative [`KError`]
/// if the caller is not a user task). Backs the `MapMemory` syscall.
///
/// The caller (the syscall layer) is responsible for deciding *which* physical
/// pages a task may map; this only performs the mapping for whoever asked.
pub fn map_device_current(dev_phys: u64) -> isize {
    // Copy the (small, `Copy`) address-space handle out under a short borrow; the
    // mapping itself touches only page tables and system registers, no switch.
    let space = {
        let sched = SCHED.lock();
        sched.tasks[sched.current[me()]].space
    };
    match space {
        Some(s) => {
            // SAFETY: at EL1 with the task's tables reachable through the linear
            // map; the syscall layer has already vetted `dev_phys` as a mappable
            // device page. The walk may need a frame for a missing table.
            let va = crate::mem::with(|frames| unsafe { s.map_device(frames, dev_phys) });
            va.map_or(KError::OutOfResources.as_raw(), |v| v as isize)
        }
        None => KError::InvalidArgument.as_raw(),
    }
}

/// Map the shared buffer (`pages` frames at `phys`) into the *current* task's
/// address space and return the resulting user virtual address (or a negative
/// [`KError`] if the caller is not a user task). Backs the `MapShared` syscall.
pub fn map_shared_current(phys: u64, pages: u32) -> isize {
    let space = {
        let sched = SCHED.lock();
        sched.tasks[sched.current[me()]].space
    };
    match space {
        Some(s) => {
            // SAFETY: at EL1 with the task's tables reachable through the linear
            // map; the frames belong to a shared object the kernel allocated. The
            // walk may need a frame for a missing table.
            let va = crate::mem::with(|frames| unsafe { s.map_shared(frames, phys, pages) });
            va.map_or(KError::OutOfResources.as_raw(), |v| v as isize)
        }
        None => KError::InvalidArgument.as_raw(),
    }
}

/// Map the DMA buffer (`pages` contiguous frames at `phys`) into the *current*
/// task's address space, non-cacheable, and return the user virtual address (or a
/// negative [`KError`] if the caller is not a user task). Backs `MapDma`.
pub fn map_dma_current(phys: u64, pages: u32) -> isize {
    let space = {
        let sched = SCHED.lock();
        sched.tasks[sched.current[me()]].space
    };
    match space {
        Some(s) => {
            // SAFETY: at EL1 with the task's tables reachable through the linear
            // map; the frames are a contiguous run the kernel allocated for this
            // DMA object. The walk may need a frame for a missing table.
            let va = crate::mem::with(|frames| unsafe { s.map_dma(frames, phys, pages) });
            va.map_or(KError::OutOfResources.as_raw(), |v| v as isize)
        }
        None => KError::InvalidArgument.as_raw(),
    }
}

/// Whether EL0 in the *current* task's address space may read — and, if `write`,
/// write — every byte of `[ptr, ptr + len)`. Returns `false` for a kernel task,
/// which has no EL0 space and so no user pointer to validate.
///
/// This is the syscall layer's guard before it dereferences a caller-supplied
/// pointer, and it walks the caller's own page tables rather than trusting a
/// range: the EL0 window is sparse, so being inside it proves nothing.
pub fn current_range_ok(ptr: u64, len: usize, write: bool) -> bool {
    let sched = SCHED.lock();
    let Some(space) = sched.tasks[sched.current[me()]].space.as_ref() else {
        return false;
    };
    // SAFETY: the space belongs to a live task, so it has not been destroyed.
    unsafe { space.user_range_ok(ptr, len, write) }
}

/// Map a fresh anonymous read/write page into the *current* task's address space
/// and return its user virtual address (or a negative [`KError`] if the caller is
/// not a user task or its memory cannot grow). Backs the `MapAnon` syscall.
///
/// The scheduler lock is taken only to *copy the space out* and later to *write
/// the bumped cursor back*, not held across the mapping itself. That matters on
/// several cores: holding it across `map_anon` (which allocates a frame and
/// broadcasts a TLB invalidate) masks interrupts and stalls every other core on
/// the scheduler lock for the whole of a slow operation — a task doing thousands
/// of `MapAnon`s serialised the entire machine. Copying is sound because a task's
/// space is touched only by that task, and a task runs on one core at a time, so
/// nothing else races the copy; the frame lock inside `map_anon` still serialises
/// the allocation itself.
pub fn map_anon_current() -> isize {
    let cpu = me();
    let (cur, mut space) = {
        let sched = SCHED.lock();
        let cur = sched.current[cpu];
        match sched.tasks[cur].space {
            Some(s) => (cur, s),
            None => return KError::InvalidArgument.as_raw(),
        }
    };
    // SAFETY: at EL1 with this (active) space's tables reachable through the
    // linear map and the frame pool mapped writable; `map_anon` only adds a page,
    // and the frame lock it takes makes the allocation atomic against other cores.
    let va = crate::mem::with(|frames| unsafe { space.map_anon(frames) });
    // Persist the bumped heap cursor. The task cannot have run elsewhere in the
    // meantime (it is mid-syscall on this core), so no update is lost.
    SCHED.lock().tasks[cur].space = Some(space);
    va.map_or(KError::OutOfResources.as_raw(), |v| v as isize)
}

/// Try to satisfy a fault at `far` by growing the current task's stack.
///
/// Returns `true` if a page was mapped and the faulting instruction should be
/// retried. Everything that decides *whether* an address is stack growth lives in
/// [`AddressSpace::grow_stack`]; this only finds the current task's space and
/// counts the successful growths (so the boot log can prove pages really arrived
/// on demand rather than being mapped up front).
pub fn grow_stack_current(far: u64) -> bool {
    let space = {
        let sched = SCHED.lock();
        match sched.tasks[sched.current[me()]].space {
            Some(s) => s,
            None => return false,
        }
    };
    // SAFETY: at EL1 in the faulting task's own (active) space, whose tables are
    // reachable through the linear map with the frame pool mapped writable;
    // `grow_stack` only adds a previously-absent page and invalidates that VA.
    let grew = crate::mem::with(|frames| unsafe { space.grow_stack(frames, far) });
    if grew {
        STACK_PAGES_GROWN.fetch_add(1, Ordering::Relaxed);
    }
    // Nothing to persist: `grow_stack` mutates the page tables, not the handle
    // (unlike `map_anon`, which bumps the heap cursor).
    grew
}

/// How many stack pages have been mapped on demand across all tasks.
static STACK_PAGES_GROWN: AtomicU64 = AtomicU64::new(0);

/// Stack pages mapped on demand so far — zero would mean demand paging never
/// actually happened and every stack was big enough up front.
#[must_use]
pub fn stack_pages_grown() -> u64 {
    STACK_PAGES_GROWN.load(Ordering::Relaxed)
}

/// The `x30`/`bl` target the task trampoline jumps to when a task's entry
/// function returns. Ends the task.
#[no_mangle]
pub extern "Rust" fn staros_task_exit() -> ! {
    exit()
}
