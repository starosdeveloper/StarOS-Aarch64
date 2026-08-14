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
use staros_arch_aarch64::{boot, exceptions, mmu, timer};
use staros_mm::PAGE_SIZE;

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
    /// Waiting for a *time*, not an event: `SleepUntil` parked this task until the
    /// monotonic clock reaches `until_ns`. Not schedulable before then, and made
    /// `Ready` by [`wake_expired`] once the deadline has passed.
    ///
    /// Deliberately a separate state from [`Blocked`], not a flavour of it. The
    /// two differ in the only way the scheduler cares about: a blocked task may
    /// wait forever for a message nobody will send, while a sleeping task *will*
    /// become runnable — the clock guarantees it. That difference is what
    /// [`Scheduler::any_runnable`] turns on, and conflating them would make the
    /// kernel decide it had finished while a task still had a wake-up coming.
    Sleeping {
        /// Monotonic nanoseconds at which this task becomes runnable again.
        until_ns: u64,
    },
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
    /// The id of the *process* this task belongs to: its own for a task that owns
    /// its address space, its creator's for a thread.
    ///
    /// A thread is a task here, with its own slot and its own id, which is what
    /// makes preemption uniform. But POSIX's `getpid` must give every thread of a
    /// program the same answer — a program that writes `/tmp/cache-<pid>` from two
    /// threads is entitled to have them agree — so the process identity is carried
    /// separately from the scheduling identity rather than derived from it.
    pid: u64,
    /// `TTBR0_EL1` value to install when switching into this task.
    ttbr0: u64,
    /// The user address space this task runs in, if any. `None` for kernel
    /// threads (they run in the base identity map). Used by `MapMemory` to add a
    /// device mapping to the *caller's* space.
    space: Option<AddressSpace>,
    /// This task's capability table: the objects it may act on, named by handle.
    caps: CapTable,
    /// Where this task enters EL0, if it is not simply the program's ELF entry on
    /// the standard stack: `(entry, stack top, argument)`.
    ///
    /// `None` for a process — it starts at `e_entry` on the one stack its address
    /// space provides. `Some` for a *thread*, which shares that space and therefore
    /// cannot share the stack: two threads growing one stack would silently write
    /// through each other. A thread's stack is ordinary anonymous memory, allocated
    /// by whoever created it, and is fixed-size — the demand-growth path belongs to
    /// the one stack region the address space knows about.
    user_start: Option<(u64, u64, u64)>,
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
    /// A notification to signal when this task exits, however it exits.
    ///
    /// The table id and not a capability: a capability is resolved through this
    /// task's own table, and by the time it is signalled that table is being torn
    /// down. Resolving once, at registration, is also what makes the promise
    /// keepable — a handle that stopped resolving between the registration and the
    /// death would turn "you will be told" into "you might be".
    death_notify: Option<usize>,
    /// Where each shared-memory object this task has mapped landed in its address
    /// space, and how far the placement cursor has advanced.
    ///
    /// One entry per *object*, not per mapping call, and that is the whole point.
    /// A file server remaps its client's one bounce buffer on every request; a
    /// display server maps a different buffer for every surface and needs them all
    /// at once. A fixed address serves the first and breaks the second; a cursor
    /// that advances on every call serves the second and makes the first climb
    /// through its address space one page per request until the page tables eat the
    /// heap. Remembering the placement serves both: the same object comes back to
    /// the same address, a new one gets the next.
    shared: SharedPlacements,
}

/// How many distinct shared buffers one task may have mapped at once.
///
/// A display server needs one per surface, so this is the ceiling on windows on
/// screen — a number that will have to grow, and is deliberately a constant here
/// rather than a `Vec` while the answer to "how many" is still a guess. The honest
/// failure at the limit is [`KError::OutOfResources`] from `MapShared`, not a
/// silently reused address.
const MAX_SHARED_MAPPINGS: usize = 16;

/// The per-task record of shared-buffer placements. See [`Task::shared`].
#[derive(Clone, Copy)]
struct SharedPlacements {
    entries: [Option<(crate::obj::ObjectRef, u64)>; MAX_SHARED_MAPPINGS],
    /// The next unused address. Grows by whole buffers, never reused: a placement
    /// outlives the mapping in the tables, because unmapping is not a thing this
    /// system does yet and pretending otherwise would hand out an address whose
    /// old pages are still there.
    cursor: u64,
}

impl SharedPlacements {
    const fn new() -> Self {
        Self {
            entries: [None; MAX_SHARED_MAPPINGS],
            cursor: staros_arch_aarch64::addrspace::USER_SHARED_VA,
        }
    }

    /// The address this object already occupies, if it has one.
    fn find(&self, obj: crate::obj::ObjectRef) -> Option<u64> {
        self.entries
            .iter()
            .flatten()
            .find(|(o, _)| *o == obj)
            .map(|(_, va)| *va)
    }

    /// Reserve an address for `obj`'s `pages`, or `None` if the table is full.
    fn place(&mut self, obj: crate::obj::ObjectRef, pages: u32) -> Option<u64> {
        let slot = self.entries.iter_mut().find(|e| e.is_none())?;
        let va = self.cursor;
        *slot = Some((obj, va));
        self.cursor += u64::from(pages) * 4096;
        Some(va)
    }
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

    /// Whether any task is `Ready`, already `Running` on some core, or `Sleeping`.
    ///
    /// Deliberately *not* counting `Blocked`: a task waiting for an event nobody
    /// will send is not work, and treating it as work is how a kernel hangs
    /// instead of finishing. This matches what the single-core scheduler did when
    /// it stopped as soon as nothing was runnable.
    ///
    /// `Sleeping` *is* counted, and that is the whole reason it is not a flavour
    /// of `Blocked`. A sleeping task has a wake-up coming from the clock, so
    /// treating it as "no work left" would end the run with a task that was about
    /// to be runnable — the demo would simply lose whatever it was going to do
    /// after its sleep, silently and only sometimes.
    fn any_runnable(&self) -> bool {
        self.tasks.iter().any(|t| {
            matches!(
                t.state,
                State::Ready | State::Running | State::Sleeping { .. }
            )
        })
    }

    /// The earliest deadline among sleeping tasks, if any are asleep.
    fn next_wake(&self) -> Option<u64> {
        self.tasks
            .iter()
            .filter_map(|t| match t.state {
                State::Sleeping { until_ns } => Some(until_ns),
                _ => None,
            })
            .min()
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

/// Create a thread in the *current* task's address space: another schedulable
/// task sharing every page and every capability, starting at EL0 `entry` with
/// `arg` in `x0`, on `stack_pages` of fresh anonymous memory, with `tls` as its
/// thread pointer. Returns the new task's id, or a negative [`KError`].
///
/// Backs the `SpawnThread` syscall, and is what `Spawn` is not: `Spawn` builds a
/// whole new address space from the init image, which is a *process*. A C runtime
/// needs the other thing — several threads over one heap — and cannot be built out
/// of processes, because the whole point is that they share memory.
///
/// Three things differ between a thread and its creator, and each is a place this
/// could go wrong:
///
/// - **The user stack.** Shared pages mean a shared stack region, and two threads
///   growing one stack write through each other. So a thread's stack is ordinary
///   anonymous memory — allocated here, fixed-size, with no demand growth.
/// - **The thread pointer.** `tls` rides in the saved context (see
///   [`CpuContext::set_tls`]), because it must change on every switch.
/// - **The capability table.** The thread gets a *copy* of its creator's. Not a
///   share: the tables are per-task arrays, and making them shared is a larger
///   change than this needs. The consequence is honest and worth knowing — a
///   capability minted *after* the thread starts is not visible to it.
pub fn spawn_thread(entry: u64, stack_pages: u64, tls: u64, arg: u64) -> isize {
    let cpu = me();
    let (cur, _old_space, caps, pid, shared) = {
        let sched = SCHED.lock();
        let cur = sched.current[cpu];
        match sched.tasks[cur].space {
            Some(s) => (
                cur,
                s,
                sched.tasks[cur].caps.clone(),
                sched.tasks[cur].pid,
                sched.tasks[cur].shared,
            ),
            None => return KError::InvalidArgument.as_raw(),
        }
    };

    // The thread's stack, out of the creator's heap region. Reserved under the
    // scheduler lock and mapped outside it, for the reason in
    // `reserve_anon_shared`: the creator is not the only task holding a copy of
    // this space's heap cursor, and two threads spawning at once would otherwise
    // hand their children the same stack.
    let Some((space, stack_base)) = reserve_anon_shared(cur, stack_pages) else {
        return KError::OutOfResources.as_raw();
    };
    // SAFETY: at EL1 with this (active) space's tables reachable through the linear
    // map; `map_anon_at` only adds pages at addresses just reserved.
    if !crate::mem::with(|frames| unsafe { space.map_anon_at(frames, stack_base, stack_pages) }) {
        return KError::OutOfResources.as_raw();
    }
    // Stacks grow down, and AArch64 requires a 16-byte aligned `sp`.
    // Stacks grow down, and AArch64 requires a 16-byte aligned `sp`.
    //
    // Worth knowing what the demo can and cannot catch here: pointing `sp` at the
    // *bottom* of the run instead of the top was falsified and passed. The reason
    // is the heap's own layout — anonymous pages are handed out consecutively, so
    // the page below a thread's stack is the creator's own memory rather than a
    // hole, and pushing into it corrupts quietly instead of faulting. A guard page
    // between heap allocations would make it observable; there is none today, and
    // saying so is more use than a check that cannot fail.
    let user_sp = (stack_base + stack_pages * PAGE_SIZE as u64) & !0xf;

    let Some(kstack) = alloc_stack() else {
        return KError::OutOfResources.as_raw();
    };
    let mut ctx = CpuContext::empty();
    ctx.init(crate::user_thread_entry, stack_top(&kstack));
    ctx.set_tls(tls);
    let Some(task) = try_box(Task {
        ctx,
        stack: kstack,
        state: State::Ready,
        id: 0,
        // The creator's process id, not a new one: that is what makes this a thread
        // of that program rather than another program.
        pid,
        ttbr0: space.ttbr0(),
        // The same space value, not a new one: `AddressSpace` is a handle, and two
        // tasks holding it is exactly what a thread is. Teardown is what has to
        // change, and does — see `exit`.
        space: Some(space),
        caps,
        user_start: Some((entry, user_sp, arg)),
        mailbox: None,
        wake_pending: false,
        on_cpu: AtomicBool::new(false),
        // Not inherited. A thread's death is not its creator's, and a registration
        // copied into every thread would fire the moment any of them ended.
        death_notify: None,
        // A *copy* of the creator's placements, exactly like the capability table
        // above and for a sharper reason: the pages are already in this address
        // space at those addresses. A thread starting with an empty table would put
        // its first new buffer on top of one its creator is using.
        shared,
    }) else {
        return KError::OutOfResources.as_raw();
    };

    let mut sched = SCHED.lock();
    if sched.tasks.try_reserve(1).is_err() {
        return KError::OutOfResources.as_raw();
    }
    let mut task = task;
    let id = sched.tasks.len() as u64;
    task.id = id;
    sched.tasks.push(task);
    drop(sched);
    crate::smp::wake_others();
    id as isize
}

/// Where the current task should enter EL0: `(entry, stack top, argument)` for a
/// thread, or `None` for a process (which starts at its ELF entry on the address
/// space's own stack).
#[must_use]
pub fn current_user_start() -> Option<(u64, u64, u64)> {
    let sched = SCHED.lock();
    sched.tasks[sched.current[me()]].user_start
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
/// Returns the new task's id, or `None` if it could not be allocated.
pub fn spawn_user(entry: extern "C" fn(), space: AddressSpace, caps: CapTable) -> Option<u64> {
    // Build the whole task *before* taking the lock. Partly to keep the lock
    // short, but mainly to keep the lock order a straight line: allocating takes
    // the heap's lock, and doing that underneath the scheduler's would nest two
    // locks for no reason.
    let stack = alloc_stack()?;
    let mut ctx = CpuContext::empty();
    ctx.init(entry, stack_top(&stack));
    let task = try_box(Task {
        ctx,
        stack,
        state: State::Ready,
        id: 0,
        pid: 0, // filled in with the task's own id below: this task owns its space

        ttbr0: space.ttbr0(),
        space: Some(space),
        caps,
        user_start: None,
        mailbox: None,
        wake_pending: false,
        on_cpu: AtomicBool::new(false),
        death_notify: None,
        shared: SharedPlacements::new(),
    })?;

    let mut sched = SCHED.lock();
    // Append rather than reuse a `Dead` slot. Reuse would need proof that the dead
    // task has left the stack, and its *slot* is still a live tombstone (task ids
    // and `current[cpu]` index into it). The heavy part — the 32 KiB kernel stack —
    // is reclaimed the moment the dead task's successor runs (`reap_dead`), which is
    // the safe point the comment used to say did not exist: the successor is, by
    // definition, off the dead stack. So a task's stack no longer waits until
    // reboot; only the small `Task` header lingers, bounded by how many tasks ran.
    if sched.tasks.try_reserve(1).is_err() {
        return None;
    }
    let mut task = task;
    let id = sched.tasks.len() as u64;
    task.id = id;
    task.pid = id;
    sched.tasks.push(task);
    drop(sched);
    // A task is runnable now — ring the doorbell so an idle core picks it up at
    // once instead of sleeping until its next tick.
    crate::smp::wake_others();
    Some(id)
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
    // Any interrupt is a chance to notice that a deadline has passed. Done before
    // the switch decision so a task whose sleep just expired can be picked on this
    // pass rather than waiting for the next interrupt.
    let woke = wake_expired();
    if NEED_RESCHED.swap(false, Ordering::Relaxed) || woke {
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
    // Announce the death *before* anything else, and before the scheduler lock is
    // taken. `notify::signal` reaches into the scheduler to wake a parked waiter,
    // so signalling while holding that lock is the deadlock the notification
    // module's own comment describes. Everything after this point is teardown that
    // cannot fail, so a server told here is never told about a task that then
    // carried on.
    let departing = {
        let mut sched = SCHED.lock();
        let cpu = me();
        let me = sched.current[cpu];
        sched.tasks[me].death_notify.take()
    };
    if let Some(id) = departing {
        crate::notify::signal(id);
    }

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
        //
        // Unless a *thread* still lives in it. Threads share one space, so the last
        // one out has to be the one that tears it down — destroying it while a
        // sibling is still running would pull the page tables out from under a live
        // task, and the fault it takes would be at some unrelated address later.
        //
        // The test is "does any other live task still name this `TTBR0`" rather
        // than a reference count kept alongside. A count is a second copy of a fact
        // the task table already holds, and the two can disagree — after a failed
        // spawn, after a task that dies before it runs. Asking the table costs a
        // scan of a few dozen entries under a lock we already hold, once per exit.
        let space = sched.tasks[prev].space.take();
        let ttbr0 = sched.tasks[prev].ttbr0;
        let shared = sched
            .tasks
            .iter()
            .enumerate()
            .any(|(i, t)| i != prev && t.state != State::Dead && t.ttbr0 == ttbr0);
        dead_space = if shared { None } else { space };
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

/// The process id of the currently running task: its own id, or its creator's if
/// it is a thread. What `getpid` in EL0 answers with.
#[must_use]
pub fn current_pid() -> u64 {
    let sched = SCHED.lock();
    let cur = sched.current[me()];
    sched.tasks[cur].pid
}

/// Arrange for `notif` to be signalled when the current task exits, or clear a
/// previous registration with `None`. Backs [`NotifyOnExit`](staros_abi::syscall::Syscall::NotifyOnExit).
///
/// One per task, last registration wins. A list would be a promise to several
/// parties, and this system has no way to say who they were once the task is
/// gone — a server that outlives its client must ask for its own notification
/// rather than share one.
pub fn set_death_notify(notif: Option<usize>) {
    let mut sched = SCHED.lock();
    let cur = sched.current[me()];
    sched.tasks[cur].death_notify = notif;
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

/// Park the current task in `parked` and switch away, resuming here when it is
/// made `Ready` again and next scheduled. The caller must already have arranged
/// for something to wake it — a wait queue for [`State::Blocked`], the clock for
/// [`State::Sleeping`]. Runs in an IRQ-masked critical section around the switch.
fn park_and_switch(parked: State) {
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
        sched.tasks[prev].state = parked;
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
    park_and_switch(State::Blocked);
    // Resumed: our mailbox holds the message a sender delivered.
    let mut sched = SCHED.lock();
    let me = sched.current[me()];
    sched.tasks[me].mailbox.take().expect("woken without a message")
}

/// Block the current task until a peer unblocks it. Called from the `Send` path
/// when the endpoint ring is full; the receiver that drains it wakes us.
pub fn block_current() {
    park_and_switch(State::Blocked);
}

/// Park the current task until a peer unblocks it, or — if `deadline_ns` is given
/// — until the monotonic clock reaches that deadline, whichever comes first.
///
/// The two wake-ups are deliberately the *same* mechanism seen from two sides: a
/// waiting task is `Sleeping { until_ns }` when it has a deadline, so
/// [`wake_expired`] can free it on time, and [`unblock`] treats `Sleeping` exactly
/// as it treats `Blocked` so a peer's signal frees it early. Having one state
/// rather than a "blocked with a timer" pair is what keeps the two paths from
/// racing over who owns the task.
pub fn block_until(deadline_ns: Option<u64>) {
    match deadline_ns {
        Some(deadline) => {
            if let Some(now) = timer::monotonic_ns() {
                if deadline <= now {
                    return;
                }
                arm_for_deadline(deadline - now);
            }
            park_and_switch(State::Sleeping { until_ns: deadline });
        }
        None => park_and_switch(State::Blocked),
    }
}

/// The monotonic clock, or 0 on a machine that has none.
///
/// Zero is the right answer for the callers this exists for — deadline
/// comparisons — because a machine with no clock cannot honour a deadline anyway,
/// and a clock frozen at zero makes every deadline lie in the future rather than
/// silently in the past. Anything that needs to *distinguish* "no clock" uses
/// `timer::monotonic_ns` directly, as `ClockNow` does.
#[must_use]
pub fn clock_now() -> u64 {
    timer::monotonic_ns().unwrap_or(0)
}

/// Park the current task until the monotonic clock reaches `deadline_ns`. Backs
/// the `SleepUntil` syscall. Returns 0 once the deadline has passed, or a negative
/// [`KError`] if this machine has no clock to sleep against.
///
/// **The deadline is absolute, and that is the point.** A relative sleep ("wake me
/// in 16 ms") drifts: the time between waking and asking again is not counted, so
/// a loop pacing itself at 60 Hz runs slower than 60 Hz by however long its own
/// work takes, and the error accumulates. An absolute deadline is also immune to
/// the race a relative one has — being preempted between computing a duration and
/// asking to sleep makes the sleep longer than intended, and there is no way for
/// the caller to notice.
///
/// A deadline already in the past returns **immediately** without parking. That is
/// what makes this usable as the timeout half of an event loop: a caller that is
/// already late must not be put to sleep for a whole scheduling round.
pub fn sleep_until(deadline_ns: u64) -> isize {
    let Some(now) = timer::monotonic_ns() else {
        return KError::NotSupported.as_raw();
    };
    if deadline_ns <= now {
        SLEEPS_ALREADY_PAST.fetch_add(1, Ordering::Relaxed);
        return 0;
    }
    // Point this core's timer at the deadline *before* parking. Without it the
    // task would still wake — on the next periodic tick — but no sooner, so every
    // sleep would round up to the tick period. That is the difference between a
    // 16 ms frame deadline and a 100 ms one.
    arm_for_deadline(deadline_ns - now);
    SLEEPS.fetch_add(1, Ordering::Relaxed);
    park_and_switch(State::Sleeping { until_ns: deadline_ns });
    0
}

/// Program this core's timer to fire in at most `nanos` from now, and never later
/// than the ordinary tick period.
///
/// **Redundant with `irq::next_interval`, and measurably so.** Breaking either one
/// alone leaves the sleep resolution intact — the tick handler re-aims for the
/// nearest deadline anyway, so a task that parked without this would still be woken
/// within a tick of asking. What this buys is the *first* window: the stretch
/// between parking and the next tick, which is up to a full period long and is
/// exactly where a short sleep lives. Both were falsified; only breaking both moved
/// the measured overshoot (13 ms to 72 ms).
///
/// Clamped below so a deadline that is nearly upon us cannot ask for an interval
/// the machine spends its whole time servicing: at a 62.5 MHz counter a single
/// tick is 16 ns, and arming that would re-enter the handler before it returned.
fn arm_for_deadline(nanos: u64) {
    /// Floor on any programmed interval. Long enough that the interrupt is taken,
    /// dispatched and returned from with room to spare; short enough to be
    /// invisible to anything a user task can perceive.
    const MIN_NANOS: u64 = 50_000; // 50 us
    let Some(ticks) = timer::ticks_from_nanos(nanos.max(MIN_NANOS)) else {
        return;
    };
    let interval = ticks.min(crate::irq::tick_interval());
    // SAFETY: at EL1 with the timer routed through the GIC; arming it again is
    // what every tick already does.
    unsafe { timer::GenericTimer::arm(interval) };
}

/// Make every sleeping task whose deadline has passed `Ready` again. Returns
/// `true` if any woke, so the caller can request a reschedule.
///
/// Called from the IRQ epilogue: any interrupt is an opportunity to notice that
/// time has passed, and the timer — whose interval [`arm_for_deadline`] shortens
/// to reach the nearest deadline — guarantees one arrives when it should.
pub fn wake_expired() -> bool {
    let Some(now) = timer::monotonic_ns() else {
        return false;
    };
    let mut sched = SCHED.lock();
    let mut woke = false;
    for task in &mut sched.tasks {
        if let State::Sleeping { until_ns } = task.state {
            if until_ns <= now {
                task.state = State::Ready;
                woke = true;
                // How late the wake-up was. This is the sleep *resolution*, and it
                // is the only number that says whether re-aiming the timer works:
                // without it every sleep would be late by up to a tick period, and
                // nothing else in the log would look any different.
                let late = now - until_ns;
                LATEST_WAKE_NS.fetch_max(late, Ordering::Relaxed);
            }
        }
    }
    if woke {
        WAKEUPS.fetch_add(1, Ordering::Relaxed);
    }
    woke
}

/// The earliest deadline any sleeping task is waiting for, if any is asleep. Read
/// by the timer tick so it can re-arm for that deadline instead of the full tick
/// period.
#[must_use]
pub fn next_wake_ns() -> Option<u64> {
    SCHED.lock().next_wake()
}

/// Sleeps that actually parked, sleeps whose deadline had already passed, and
/// wake-ups performed. All three are the claim: zero parks would mean the demo
/// never slept, and zero already-past would mean the "do not sleep if late" path
/// was never taken.
static SLEEPS: AtomicU64 = AtomicU64::new(0);
static SLEEPS_ALREADY_PAST: AtomicU64 = AtomicU64::new(0);
static WAKEUPS: AtomicU64 = AtomicU64::new(0);
/// The worst overshoot any sleeper saw: how long after its deadline it was made
/// runnable. The sleep resolution, measured rather than assumed.
static LATEST_WAKE_NS: AtomicU64 = AtomicU64::new(0);

/// `(parked, already past, wake-ups, worst overshoot in ns)` — see [`SLEEPS`].
#[must_use]
pub fn sleep_counts() -> (u64, u64, u64, u64) {
    (
        SLEEPS.load(Ordering::Relaxed),
        SLEEPS_ALREADY_PAST.load(Ordering::Relaxed),
        WAKEUPS.load(Ordering::Relaxed),
        LATEST_WAKE_NS.load(Ordering::Relaxed),
    )
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
        // `Sleeping` counts as parked here, not just `Blocked`: a task waiting on
        // `WaitAny` with a timeout is sleeping against its deadline *and* standing
        // in wait queues, and a signal must free it early. A task that is merely
        // sleeping (`SleepUntil`) is in no queue, so nothing signals it — the two
        // cannot be confused by accident.
        if matches!(
            sched.tasks[task].state,
            State::Blocked | State::Sleeping { .. }
        ) {
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
pub fn map_shared_current(obj: crate::obj::ObjectRef, phys: u64, pages: u32) -> isize {
    // Decide the address under the lock, map outside it: the walk may allocate a
    // table frame, and the frame pool must never be entered holding this lock.
    let (space, base, already) = {
        let mut sched = SCHED.lock();
        let cur = sched.current[me()];
        let space = sched.tasks[cur].space;
        match sched.tasks[cur].shared.find(obj) {
            // Mapped already: the same object gives the same address, and the
            // mapping is still there, so there is nothing left to do.
            Some(va) => (space, va, true),
            None => match sched.tasks[cur].shared.place(obj, pages) {
                Some(va) => (space, va, false),
                None => return KError::OutOfResources.as_raw(),
            },
        }
    };
    if already {
        return base as isize;
    }
    match space {
        Some(s) => {
            // SAFETY: at EL1 with the task's tables reachable through the linear
            // map; the frames belong to a shared object the kernel allocated. The
            // walk may need a frame for a missing table.
            let va = crate::mem::with(|frames| unsafe { s.map_shared(frames, base, phys, pages) });
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

/// Map `pages` fresh anonymous read/write pages into the *current* task's address
/// space and return the user virtual address of the first (or a negative
/// [`KError`] if the caller is not a user task or its memory cannot grow). Backs
/// the `MapAnon` syscall.
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
pub fn map_anon_current(pages: u64) -> isize {
    let cpu = me();
    let cur = {
        let sched = SCHED.lock();
        sched.current[cpu]
    };
    let Some((space, first_va)) = reserve_anon_shared(cur, pages) else {
        return KError::OutOfResources.as_raw();
    };
    // Done in chunks, with interrupts let through between them. A syscall runs with
    // IRQs masked from the moment the exception is taken until it returns, so a
    // single call that mapped 1024 pages — a page zeroed and a TLB entry
    // invalidated each — held off the timer for the whole of it. Nothing was lost
    // by that, but nothing could be *scheduled* either: preemption stopped, and a
    // task sleeping on a 20 ms deadline could not be woken until this call
    // finished. That is exactly the latency an event loop cannot have, and it was
    // invisible until `SleepUntil` existed to measure it.
    //
    // The chunk size is the trade: small enough that the masked stretch stays
    // short, large enough that the window costs little. Measured on the QEMU
    // `virt` demo, worst sleep overshoot against a 20 ms deadline: 708 ms with no
    // chunking at all, 42 ms at 64 pages, 13 ms at 16. The demo's runtime did not
    // move, so the windows themselves cost nothing measurable — 16 it is.
    const CHUNK: u64 = 16;
    let mut mapped = 0;
    while mapped < pages {
        let chunk = CHUNK.min(pages - mapped);
        let va = first_va + mapped * PAGE_SIZE as u64;
        // SAFETY: at EL1 with this (active) space's tables reachable through the
        // linear map and the frame pool mapped writable; `map_anon_at` only adds
        // pages at addresses this call reserved, and the frame lock it takes makes
        // the allocation atomic against other cores.
        if !crate::mem::with(|frames| unsafe { space.map_anon_at(frames, va, chunk) }) {
            break;
        }
        mapped += chunk;
        if mapped < pages {
            // Open a window. Any pending interrupt is taken here — which may
            // reschedule this core and resume us later. That is safe because
            // `space` is a *copy*: only this task touches its own heap cursor, and
            // it is running on this core, mid-syscall, until this returns.
            // SAFETY: at EL1 with vectors and the GIC up; the mask is restored
            // immediately, so the caller's masked section resumes unchanged.
            unsafe {
                exceptions::enable_irqs();
                exceptions::disable_irqs();
            }
        }
    }
    // The cursor was already published to every task in this space by the
    // reservation; nothing to persist here.
    //
    // A partial run is a failure, even though the pages it did map stay mapped and
    // are reclaimed at teardown: handing back an address for a run shorter than
    // asked would be read as the whole thing.
    if mapped == pages {
        first_va as isize
    } else {
        KError::OutOfResources.as_raw()
    }
}

/// Reserve `pages` of anonymous address space for the task at index `cur`, and
/// publish the new cursor to **every task sharing that address space**.
///
/// This exists because `AddressSpace` is a `Copy` handle and a thread is a second
/// task holding a copy of it. The heap cursor lives in the handle, so two threads
/// each bumping their own copy reserve the *same* addresses: the second `malloc` in
/// a threaded program hands out memory the first one is already using. The symptom
/// is not a crash but data that changes under a thread that never wrote it, and it
/// only appears once two cores run the same process at once.
///
/// The whole read-modify-write happens under the scheduler lock, which is what
/// makes concurrent reservations disjoint. Mapping the pages is left outside it.
fn reserve_anon_shared(cur: usize, pages: u64) -> Option<(AddressSpace, u64)> {
    let mut sched = SCHED.lock();
    let ttbr0 = sched.tasks.get(cur)?.ttbr0;
    // The authoritative cursor is the furthest any task in this space has reached.
    let top = sched
        .tasks
        .iter()
        .filter(|t| t.ttbr0 == ttbr0)
        .filter_map(|t| t.space.as_ref().map(AddressSpace::heap_next))
        .max()?;
    let mut space = sched.tasks.get(cur)?.space?;
    space.set_heap_next(top);
    let va = space.reserve_anon(pages)?;
    let next = space.heap_next();
    for task in sched.tasks.iter_mut().filter(|t| t.ttbr0 == ttbr0) {
        if let Some(s) = task.space.as_mut() {
            s.set_heap_next(next);
        }
    }
    Some((space, va))
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
