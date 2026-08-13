//! Layer 5 of the contract: threads, mutexes, condition variables and TLS.
//!
//! The kernel already had everything this needs — `SpawnThread` starts a task in
//! the caller's address space with a thread pointer of the caller's choosing,
//! notifications carry a signal that is *counted* rather than lost, and `WaitAny`
//! adds a deadline. What was missing is the shape POSIX expects, and one decision
//! the kernel cannot make: where thread-local variables live.
//!
//! ## Thread-local storage
//! AArch64 uses TLS variant I: `TPIDR_EL0` points at a 16-byte TCB and the static
//! TLS block follows immediately after it, so a local-exec access compiles to
//! `mrs x, tpidr_el0` plus a constant offset the linker computed. This library
//! therefore has to hand every thread a block laid out exactly that way:
//!
//! ```text
//! TPIDR_EL0 -> +0   reserved (a dynamic loader's DTV; there is none here)
//!              +8   this thread's control block
//!              +16  a copy of .tdata, then zeroed .tbss   <- the linker's offsets
//! ```
//!
//! The template comes from `__tdata_start`/`__tdata_end`/`__tbss_end`, which
//! `services/init/boot/image.ld` exports. Nothing guesses the size: a program that
//! assumed one would be correct until somebody declared another `_Thread_local`.
//!
//! The second word is what makes `pthread_self` and `pthread_getspecific` a load
//! rather than a search, and it is why the TCB is ours to define at all.
//!
//! ## Why the notifications are allocated up front
//! A capability table is **copied** into a new thread when it is created. A
//! notification created before any thread exists therefore has the same handle in
//! every thread — and one created afterwards exists only in its creator, where
//! nobody else can signal it. So the parkers live in a pool built during start-up:
//! that, not tidiness, is why `MAX_THREADS` is a fixed number.
//!
//! ## Waking rules, and why stale wake-ups are safe
//! Every park in this file sits under a loop that re-checks the thing it waited
//! for, and `pthread_cond_wait` returns to its caller (whose own predicate loop is
//! required by POSIX). A signal that arrives for a thread which has already made
//! progress is therefore harmless: it becomes a pending count that makes some later
//! park return early. Building on that fact is what lets `unlock` hand the whole
//! waiter queue a wake-up without tracking who actually needs one.

use core::ffi::{c_int, c_void};
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};

use crate::sys;

/// How many threads may exist at once, including `main`.
///
/// Fixed because the parker pool is: see the module docs. A program that asks for
/// one more gets `EAGAIN`, which is a POSIX answer, rather than a thread whose
/// wake-ups go nowhere.
pub(crate) const MAX_THREADS: usize = 32;

/// How many `pthread_key_t`s a program may create. Qt uses a handful.
const MAX_KEYS: usize = 32;

/// Pages of stack per thread. The kernel maps them on `SpawnThread` and there is no
/// way to give them back — see the note on `pthread_join`.
const STACK_PAGES: u64 = 16;

/// Bytes of TCB in front of the TLS block, fixed by the AArch64 ABI.
const TCB_SIZE: usize = 16;

extern "C" {
    /// The TLS template, from the linker script.
    static __tdata_start: u8;
    static __tdata_end: u8;
    static __tbss_end: u8;
}

/// One thread's control block. `pthread_t` is a pointer to this.
#[repr(C)]
pub struct Thread {
    /// The notification this thread parks on, and the slot it came from.
    parker: u32,
    slot: u32,
    /// Set when the thread has run to completion.
    finished: AtomicU32,
    /// Set when nobody will join: the thread frees itself.
    detached: AtomicU32,
    /// Whoever is blocked in `pthread_join` on us, to be woken at the end.
    joiner: AtomicPtr<Thread>,
    /// What the start routine returned.
    retval: AtomicPtr<c_void>,
    /// Intrusive link for the queue of whichever mutex, condition variable or
    /// semaphore this thread is waiting on. One at a time, by construction: a
    /// thread waits for exactly one thing.
    next: AtomicPtr<Thread>,
    /// Whether this thread is currently on *some* queue.
    ///
    /// There is one `next` field per thread, so a thread that got queued twice
    /// would link to itself and turn the list into a ring — and the first walk of
    /// that ring never ends. It is easy to do by accident: a waiter that enqueues,
    /// then wins the lock on the re-check, is still in the queue when it next
    /// blocks. This flag makes the second push a no-op, and taking a thread off a
    /// queue clears it.
    queued: AtomicU32,
    /// The TLS block (the allocation, not the thread pointer).
    tls_block: *mut u8,
    /// `pthread_setspecific` values.
    keys: [*mut c_void; MAX_KEYS],
    /// What to run, for a thread that has not started yet.
    start: Option<extern "C" fn(*mut c_void) -> *mut c_void>,
    arg: *mut c_void,
}

/// The parker pool, created before the first thread so every thread's copy of the
/// capability table names the same notifications.
static mut PARKERS: [u32; MAX_THREADS] = [0; MAX_THREADS];
/// Which pool slots are taken. Index 0 belongs to `main`.
static SLOTS: [AtomicU32; MAX_THREADS] = [const { AtomicU32::new(0) }; MAX_THREADS];
/// How many keys have been handed out.
static KEYS_USED: AtomicUsize = AtomicUsize::new(0);
/// Destructors are accepted and never run — see `pthread_key_create`.
static KEY_DTORS: [AtomicPtr<c_void>; MAX_KEYS] =
    [const { AtomicPtr::new(ptr::null_mut()) }; MAX_KEYS];

/// Qt reads this to skip locking while a program is single-threaded. It stops being
/// true at the first `pthread_create` and never becomes true again — a program that
/// joins every thread is still one that *had* threads, and code which cached the
/// value would be wrong.
#[no_mangle]
pub static mut __libc_single_threaded: c_int = 1;

/// Build the parker pool and give `main` its thread block. Called from `_start`
/// before `main`, and before anything can create a thread.
pub(crate) fn init() {
    for i in 0..MAX_THREADS {
        let Some(handle) = sys::notify_create() else {
            // Out of notifications: the threads that fit still work, and
            // `pthread_create` past this point fails with EAGAIN rather than
            // handing out a parker nobody can signal.
            break;
        };
        // SAFETY: start-up, before any thread exists.
        unsafe { (*ptr::addr_of_mut!(PARKERS))[i] = handle };
    }

    // `main`'s own block. Its TLS is set here, not by the kernel: `TPIDR_EL0` is
    // writable at EL0, so a thread can install its own thread pointer, and only
    // *new* threads need the kernel's help (it must be set before their first
    // instruction).
    if let Some(main_thread) = new_thread_block() {
        // SAFETY: the block was just built for this thread.
        unsafe { install(main_thread) };
    }
}

/// Take a free parker slot, or `None` when the pool is exhausted.
fn take_slot() -> Option<u32> {
    for (i, slot) in SLOTS.iter().enumerate() {
        if slot
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // SAFETY: written once during `init`, read-only afterwards.
            let handle = unsafe { (*ptr::addr_of!(PARKERS))[i] };
            if handle == 0 {
                slot.store(0, Ordering::Release);
                return None;
            }
            return Some(i as u32);
        }
    }
    None
}

/// Allocate a thread control block and its TLS block, laid out as the ABI requires.
fn new_thread_block() -> Option<*mut Thread> {
    let slot = take_slot()?;
    // SAFETY: `PARKERS` is written once in `init`.
    let parker = unsafe { (*ptr::addr_of!(PARKERS))[slot as usize] };

    // The template: initialised bytes, then the zeroed tail. These are
    // linker-provided addresses in our own image.
    let (tdata, tdata_len, tls_len) = {
        let start = ptr::addr_of!(__tdata_start) as usize;
        let init_end = ptr::addr_of!(__tdata_end) as usize;
        let end = ptr::addr_of!(__tbss_end) as usize;
        (start as *const u8, init_end - start, end - start)
    };

    let block = crate::heap_alloc(TCB_SIZE + tls_len, 16);
    if block.is_null() {
        SLOTS[slot as usize].store(0, Ordering::Release);
        return None;
    }
    // SAFETY: `block` is `TCB_SIZE + tls_len` bytes we just allocated.
    unsafe {
        ptr::write_bytes(block, 0, TCB_SIZE + tls_len);
        if tdata_len > 0 {
            ptr::copy_nonoverlapping(tdata, block.add(TCB_SIZE), tdata_len);
        }
    }

    let thread = crate::heap_alloc(core::mem::size_of::<Thread>(), 16).cast::<Thread>();
    if thread.is_null() {
        SLOTS[slot as usize].store(0, Ordering::Release);
        return None;
    }
    // SAFETY: freshly allocated and uniquely ours.
    unsafe {
        thread.write(Thread {
            parker,
            slot,
            finished: AtomicU32::new(0),
            detached: AtomicU32::new(0),
            joiner: AtomicPtr::new(ptr::null_mut()),
            retval: AtomicPtr::new(ptr::null_mut()),
            next: AtomicPtr::new(ptr::null_mut()),
            queued: AtomicU32::new(0),
            tls_block: block,
            keys: [ptr::null_mut(); MAX_KEYS],
            start: None,
            arg: ptr::null_mut(),
        });
        // The TCB's second word points back here; that is what `pthread_self` reads.
        block.cast::<usize>().add(1).write(thread as usize);
    }
    Some(thread)
}

/// Install a thread block as *this* thread's, by writing the thread pointer.
///
/// # Safety
/// `thread` must be a block built by [`new_thread_block`] and not installed in any
/// other thread.
unsafe fn install(thread: *mut Thread) {
    // SAFETY: forwarded; `TPIDR_EL0` is writable at EL0 and this is its whole
    // purpose.
    unsafe {
        let tp = (*thread).tls_block;
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!("msr tpidr_el0, {tp}", tp = in(reg) tp, options(nomem, nostack));
        let _ = tp;
    }
}

/// The thread pointer, or null before `init` has run.
fn thread_pointer() -> *mut u8 {
    #[cfg(target_arch = "aarch64")]
    {
        let tp: usize;
        // SAFETY: reading the thread pointer has no side effects.
        unsafe { core::arch::asm!("mrs {tp}, tpidr_el0", tp = out(reg) tp, options(nomem, nostack)) };
        tp as *mut u8
    }
    #[cfg(not(target_arch = "aarch64"))]
    ptr::null_mut()
}

/// This thread's control block, or null if it has none (which cannot happen after
/// `init` short of the pool being exhausted at start-up).
pub(crate) fn current() -> *mut Thread {
    let tp = thread_pointer();
    if tp.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: the TCB's second word is written when the block is built.
    unsafe { tp.cast::<usize>().add(1).read() as *mut Thread }
}

impl Thread {
    /// Sleep until somebody unparks us.
    fn park(&self) {
        sys::wait(self.parker);
    }

    /// Sleep until somebody unparks us or the absolute deadline passes.
    fn park_until(&self, deadline_ns: u64) -> bool {
        sys::wait_until(self.parker, deadline_ns)
    }

    /// Wake a parked thread. Safe to call when it is not parked: the signal is
    /// counted and makes its next park return early, and every park here is under a
    /// loop that re-checks its condition.
    fn unpark(&self) {
        sys::notify_signal(self.parker);
    }
}

/// A spin lock over the queue fields of a mutex/condvar/semaphore.
///
/// Held for a few instructions at a time — long enough to push or take a list, and
/// never across a syscall. `Yield` rather than a bare spin because this system
/// preempts: on one core, spinning without yielding against a lock holder that has
/// been preempted burns a whole timeslice every time.
struct QueueLock<'a>(&'a AtomicU32);

impl<'a> QueueLock<'a> {
    fn acquire(lock: &'a AtomicU32) -> Self {
        while lock
            .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            sys::yield_now();
        }
        Self(lock)
    }
}

impl Drop for QueueLock<'_> {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Release);
    }
}

/// A queue of parked threads, intrusive through [`Thread::next`].
#[repr(C)]
pub struct Queue {
    lock: AtomicU32,
    head: AtomicPtr<Thread>,
}

impl Queue {
    /// Add a thread to the queue, unless it is already on one.
    fn push(&self, thread: *mut Thread) {
        let _guard = QueueLock::acquire(&self.lock);
        // SAFETY: `thread` is a live control block; `next` and `queued` are only
        // touched under this lock.
        unsafe {
            if (*thread).queued.swap(1, Ordering::AcqRel) == 1 {
                return;
            }
            (*thread).next.store(self.head.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        self.head.store(thread, Ordering::Release);
    }

    /// Take the whole queue, marking every thread in it as no longer queued.
    fn take(&self) -> *mut Thread {
        let _guard = QueueLock::acquire(&self.lock);
        let head = self.head.swap(ptr::null_mut(), Ordering::AcqRel);
        let mut thread = head;
        while !thread.is_null() {
            // SAFETY: the list is this queue's, walked under its lock.
            unsafe {
                (*thread).queued.store(0, Ordering::Release);
                thread = (*thread).next.load(Ordering::Relaxed);
            }
        }
        head
    }

    /// Take one thread, or null.
    fn pop(&self) -> *mut Thread {
        let _guard = QueueLock::acquire(&self.lock);
        let head = self.head.load(Ordering::Acquire);
        if !head.is_null() {
            // SAFETY: `head` is a live control block linked under this lock.
            unsafe {
                let next = (*head).next.load(Ordering::Relaxed);
                (*head).queued.store(0, Ordering::Release);
                self.head.store(next, Ordering::Release);
            }
        }
        head
    }

    /// Wake every thread in a list already taken from a queue.
    fn wake_all(mut thread: *mut Thread) {
        while !thread.is_null() {
            // SAFETY: the list came from a queue and every link is a live block.
            let next = unsafe { (*thread).next.load(Ordering::Relaxed) };
            // SAFETY: as above.
            unsafe { (*thread).unpark() };
            thread = next;
        }
    }
}

/// A mutex, as C sees it. All-zero is a valid unlocked mutex, so
/// `PTHREAD_MUTEX_INITIALIZER` is `{0}` and a static needs no constructor.
#[repr(C)]
pub struct Mutex {
    /// 0 unlocked, 1 locked.
    state: AtomicU32,
    queue: Queue,
    _pad: usize,
}

impl Mutex {
    fn try_lock(&self) -> bool {
        self.state
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    fn lock(&self) {
        loop {
            if self.try_lock() {
                return;
            }
            let me = current();
            if me.is_null() {
                // No thread block: spin. Only reachable before `init`.
                sys::yield_now();
                continue;
            }
            // Enqueue *before* the last check, so an unlock that happens in the
            // window between the check and the park still finds us and signals —
            // and the signal is counted even if it lands before we park.
            self.queue.push(me);
            if self.try_lock() {
                // We are holding the lock *and* still queued. Leaving it there
                // would let our own `unlock` wake us instead of a real waiter, who
                // would then sleep with nobody left to wake them. Draining the
                // queue removes the stale entry and costs a wake-up to threads that
                // were going to retry anyway. This path only runs when the lock was
                // released inside the window above.
                Queue::wake_all(self.queue.take());
                return;
            }
            // SAFETY: `me` is this thread's own block.
            unsafe { (*me).park() };
        }
    }

    fn unlock(&self) {
        self.state.store(0, Ordering::Release);
        // One waiter, not all of them. Waking the whole queue is simpler and was
        // what this did first; it is also `n` syscalls per unlock, and with four
        // threads and a few thousand acquisitions that turned a demo into
        // something slower than the machine it was measuring. Handing off to one is
        // safe because the only way a queued thread is *not* going to park is the
        // drain above, which removes it.
        let thread = self.queue.pop();
        if !thread.is_null() {
            // SAFETY: a live control block taken from this queue.
            unsafe { (*thread).unpark() };
        }
    }
}

/// A condition variable.
#[repr(C)]
pub struct Cond {
    queue: Queue,
    _pad: [usize; 2],
}

/// A counting semaphore.
#[repr(C)]
pub struct Sem {
    count: AtomicU32,
    queue: Queue,
    _pad: usize,
}

/// A reader/writer lock.
///
/// POSIX gives readers and writers one `unlock`, so the lock has to remember which
/// kind the caller holds: `owner` is the writer's control block, or null while only
/// readers are inside. Without it `pthread_rwlock_unlock` would have to guess, and
/// it would guess wrong exactly when a writer unlocks with readers queued.
///
/// No writer preference. A stream of readers can starve a writer here, which is
/// worth knowing and not worth fixing until something measures it.
#[repr(C)]
pub struct RwLock {
    readers: AtomicU32,
    writer: AtomicU32,
    owner: AtomicPtr<Thread>,
    queue: Queue,
}

impl RwLock {
    fn read_lock(&self) {
        loop {
            if self.writer.load(Ordering::Acquire) == 0 {
                self.readers.fetch_add(1, Ordering::AcqRel);
                if self.writer.load(Ordering::Acquire) == 0 {
                    return;
                }
                // A writer took it in the window; back out and wait rather than
                // reading under a writer.
                self.readers.fetch_sub(1, Ordering::AcqRel);
            }
            let me = current();
            if me.is_null() {
                sys::yield_now();
                continue;
            }
            self.queue.push(me);
            if self.writer.load(Ordering::Acquire) != 0 {
                // SAFETY: our own block.
                unsafe { (*me).park() };
            }
        }
    }

    fn write_lock(&self) {
        loop {
            if self
                .writer
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                if self.readers.load(Ordering::Acquire) == 0 {
                    self.owner.store(current(), Ordering::Release);
                    return;
                }
                self.writer.store(0, Ordering::Release);
                Queue::wake_all(self.queue.take());
            }
            let me = current();
            if me.is_null() {
                sys::yield_now();
                continue;
            }
            self.queue.push(me);
            if self.writer.load(Ordering::Acquire) != 0
                || self.readers.load(Ordering::Acquire) != 0
            {
                // SAFETY: our own block.
                unsafe { (*me).park() };
            }
        }
    }

    fn unlock(&self) {
        if self.owner.load(Ordering::Acquire) == current() && self.writer.load(Ordering::Acquire) == 1
        {
            self.owner.store(ptr::null_mut(), Ordering::Release);
            self.writer.store(0, Ordering::Release);
        } else {
            self.readers.fetch_sub(1, Ordering::AcqRel);
        }
        Queue::wake_all(self.queue.take());
    }
}

/// The trampoline every thread starts at: run the routine, publish the result, wake
/// the joiner, end the task.
///
/// # Safety
/// Called by the kernel with the argument `pthread_create` passed.
extern "C" fn thread_entry(arg: *mut c_void) -> ! {
    let thread = arg.cast::<Thread>();
    // SAFETY: `arg` is the control block this thread was created with; the kernel
    // already installed our thread pointer, so `current()` agrees with it.
    let (start, argument) = unsafe { ((*thread).start, (*thread).arg) };
    let retval = match start {
        Some(f) => f(argument),
        None => ptr::null_mut(),
    };
    // SAFETY: as above.
    unsafe {
        (*thread).retval.store(retval, Ordering::Release);
        (*thread).finished.store(1, Ordering::Release);
        let joiner = (*thread).joiner.load(Ordering::Acquire);
        if !joiner.is_null() {
            (*joiner).unpark();
        }
        if (*thread).detached.load(Ordering::Acquire) == 1 {
            // Nobody will collect us. The parker slot goes back to the pool; the
            // block itself and the TLS stay allocated, because freeing memory this
            // thread is still running on is a race with nothing to win.
            SLOTS[(*thread).slot as usize].store(0, Ordering::Release);
        }
    }
    sys::exit(0)
}

/// The C entry points.
#[cfg(not(test))]
pub mod exports {
    use super::{
        c_int, c_void, current, new_thread_block, ptr, sys, thread_entry, Cond, Mutex, Ordering,
        Queue, RwLock, Sem, Thread, KEYS_USED, KEY_DTORS, MAX_KEYS, SLOTS, STACK_PAGES,
    };

    /// `EAGAIN`, the errno POSIX gives for "no resources for another thread".
    const EAGAIN: c_int = 11;
    /// `ETIMEDOUT`, for the timed waits.
    const ETIMEDOUT: c_int = 110;
    /// `EINVAL`.
    const EINVAL: c_int = 22;
    /// `EBUSY`, for `pthread_mutex_trylock`.
    const EBUSY: c_int = 16;
    /// `ENOSYS`, for the things this system genuinely does not have.
    const ENOSYS: c_int = 38;

    /// `pthread_attr_t` as this library defines it: a stack size and a detach flag.
    #[repr(C)]
    pub struct Attr {
        stack_pages: u64,
        detached: c_int,
        _pad: [u64; 2],
    }

    /// # Safety
    /// C ABI: `thread` receives the new thread's id; `attr` may be null.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_create(
        thread: *mut usize,
        attr: *const Attr,
        start: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int {
        let Some(block) = new_thread_block() else {
            return EAGAIN;
        };
        // SAFETY: freshly built and not yet running.
        unsafe {
            (*block).start = Some(start);
            (*block).arg = arg;
            if !attr.is_null() && (*attr).detached != 0 {
                (*block).detached.store(1, Ordering::Release);
            }
        }
        // SAFETY: `attr` is either null or a `pthread_attr_t` we defined.
        let pages = if attr.is_null() {
            STACK_PAGES
        } else {
            unsafe { (*attr).stack_pages.max(1) }
        };

        // From here a second thread exists, and anything that cached "single
        // threaded" is wrong. Published *before* the thread starts.
        // SAFETY: a plain store to a static the C side only reads.
        unsafe { core::ptr::write_volatile(ptr::addr_of_mut!(super::__libc_single_threaded), 0) };

        // SAFETY: `thread_entry` is code in this image and the TLS block belongs to
        // the thread being created.
        let rc = unsafe {
            sys::spawn_thread(
                thread_entry as *const () as usize as u64,
                pages,
                (*block).tls_block as u64,
                block as u64,
            )
        };
        if rc < 0 {
            // SAFETY: nothing else can see the block yet.
            unsafe { SLOTS[(*block).slot as usize].store(0, Ordering::Release) };
            return EAGAIN;
        }
        if !thread.is_null() {
            // SAFETY: forwarded from the caller.
            unsafe { *thread = block as usize };
        }
        0
    }

    /// # Safety
    /// C ABI: `id` names a thread that has not been joined or detached.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_join(id: usize, retval: *mut *mut c_void) -> c_int {
        let target = id as *mut Thread;
        if target.is_null() {
            return EINVAL;
        }
        let me = current();
        if me.is_null() {
            return EINVAL;
        }
        // SAFETY: both are live control blocks.
        unsafe {
            (*target).joiner.store(me, Ordering::Release);
            // Re-check after registering: the thread may have finished in the
            // window, in which case nobody will ever unpark us.
            while (*target).finished.load(Ordering::Acquire) == 0 {
                (*me).park();
            }
            if !retval.is_null() {
                *retval = (*target).retval.load(Ordering::Acquire);
            }
            // The parker slot returns to the pool; the control block and the TLS
            // block do not, and neither does the stack — `MapAnon` has no
            // counterpart in the ABI yet, so a thread's pages are spent for the
            // life of the process. Qt creates a handful of threads and keeps them,
            // so this is a real limit rather than an immediate problem, and it is
            // written down in docs/LIBC-CONTRACT.md.
            SLOTS[(*target).slot as usize].store(0, Ordering::Release);
        }
        0
    }

    #[no_mangle]
    pub extern "C" fn pthread_detach(id: usize) -> c_int {
        let target = id as *mut Thread;
        if target.is_null() {
            return EINVAL;
        }
        // SAFETY: a live control block.
        unsafe { (*target).detached.store(1, Ordering::Release) };
        0
    }

    #[no_mangle]
    pub extern "C" fn pthread_self() -> usize {
        current() as usize
    }

    #[no_mangle]
    pub extern "C" fn pthread_equal(a: usize, b: usize) -> c_int {
        c_int::from(a == b)
    }

    /// # Safety
    /// C ABI: `retval` is this thread's result.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_exit(retval: *mut c_void) -> ! {
        let me = current();
        if !me.is_null() {
            // SAFETY: our own block.
            unsafe {
                (*me).retval.store(retval, Ordering::Release);
                (*me).finished.store(1, Ordering::Release);
                let joiner = (*me).joiner.load(Ordering::Acquire);
                if !joiner.is_null() {
                    (*joiner).unpark();
                }
            }
        }
        sys::exit(0)
    }

    // ---- mutexes -----------------------------------------------------------

    /// # Safety
    /// C ABI: `m` points at a `pthread_mutex_t`.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_mutex_init(m: *mut Mutex, _attr: *const c_void) -> c_int {
        // SAFETY: forwarded from the caller. All-zero is the unlocked state, which
        // is also what `PTHREAD_MUTEX_INITIALIZER` produces — the two paths agree by
        // construction rather than by convention.
        unsafe { ptr::write_bytes(m.cast::<u8>(), 0, core::mem::size_of::<Mutex>()) };
        0
    }

    /// # Safety
    /// As [`pthread_mutex_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_mutex_destroy(_m: *mut Mutex) -> c_int {
        0
    }

    /// # Safety
    /// As [`pthread_mutex_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_mutex_lock(m: *mut Mutex) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { (*m).lock() };
        0
    }

    /// # Safety
    /// As [`pthread_mutex_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_mutex_trylock(m: *mut Mutex) -> c_int {
        // SAFETY: forwarded from the caller.
        if unsafe { (*m).try_lock() } {
            0
        } else {
            EBUSY
        }
    }

    /// # Safety
    /// As [`pthread_mutex_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_mutex_unlock(m: *mut Mutex) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { (*m).unlock() };
        0
    }

    // ---- condition variables ------------------------------------------------

    /// # Safety
    /// C ABI: `c` points at a `pthread_cond_t`.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_cond_init(c: *mut Cond, _attr: *const c_void) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { ptr::write_bytes(c.cast::<u8>(), 0, core::mem::size_of::<Cond>()) };
        0
    }

    /// # Safety
    /// As [`pthread_cond_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_cond_destroy(_c: *mut Cond) -> c_int {
        0
    }

    /// # Safety
    /// As [`pthread_cond_init`]; `m` must be held by this thread.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_cond_wait(c: *mut Cond, m: *mut Mutex) -> c_int {
        let me = current();
        if me.is_null() {
            return EINVAL;
        }
        // Queue up *before* releasing the mutex. The other order is the classic
        // lost-wakeup: a signal sent between the unlock and the enqueue would find
        // an empty queue and wake nobody.
        // SAFETY: forwarded from the caller.
        unsafe {
            (*c).queue.push(me);
            (*m).unlock();
            (*me).park();
            (*m).lock();
        }
        0
    }

    /// # Safety
    /// As [`pthread_cond_wait`]; `abstime` is an absolute `timespec`.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_cond_timedwait(
        c: *mut Cond,
        m: *mut Mutex,
        abstime: *const crate::time::Timespec,
    ) -> c_int {
        let me = current();
        if me.is_null() || abstime.is_null() {
            return EINVAL;
        }
        // SAFETY: forwarded from the caller.
        let deadline = unsafe { crate::time::join((*abstime).tv_sec, (*abstime).tv_nsec) };
        // SAFETY: forwarded from the caller.
        let signalled = unsafe {
            (*c).queue.push(me);
            (*m).unlock();
            let signalled = (*me).park_until(deadline);
            (*m).lock();
            signalled
        };
        if signalled {
            0
        } else {
            ETIMEDOUT
        }
    }

    /// # Safety
    /// As [`pthread_cond_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_cond_signal(c: *mut Cond) -> c_int {
        // SAFETY: forwarded from the caller.
        let thread = unsafe { (*c).queue.pop() };
        if !thread.is_null() {
            // SAFETY: a live control block taken from the queue.
            unsafe { (*thread).unpark() };
        }
        0
    }

    /// # Safety
    /// As [`pthread_cond_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_cond_broadcast(c: *mut Cond) -> c_int {
        // SAFETY: forwarded from the caller.
        Queue::wake_all(unsafe { (*c).queue.take() });
        0
    }

    // ---- once, keys ---------------------------------------------------------

    /// # Safety
    /// C ABI: `once` points at a `pthread_once_t` (an `int`, zero-initialised).
    #[no_mangle]
    pub unsafe extern "C" fn pthread_once(once: *mut u32, routine: extern "C" fn()) -> c_int {
        // SAFETY: forwarded from the caller; the word is only ever touched through
        // these atomic operations.
        let state = unsafe { &*once.cast::<core::sync::atomic::AtomicU32>() };
        match state.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                routine();
                state.store(2, Ordering::Release);
            }
            Err(_) => {
                // Somebody else is running it. Waiting by yielding rather than
                // parking: `pthread_once` has no queue to be woken from, and the
                // routine is short by definition.
                while state.load(Ordering::Acquire) != 2 {
                    sys::yield_now();
                }
            }
        }
        0
    }

    /// # Safety
    /// C ABI: `key` receives the new key.
    ///
    /// The destructor is recorded and never called: running it would mean walking
    /// every thread's key array at exit, and a thread that exits here does not run
    /// any cleanup at all yet. Accepting it silently and forgetting it would be
    /// worse than not accepting it, so it is written down here and in
    /// docs/LIBC-CONTRACT.md.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_key_create(
        key: *mut c_int,
        dtor: *mut c_void,
    ) -> c_int {
        let index = KEYS_USED.fetch_add(1, Ordering::AcqRel);
        if index >= MAX_KEYS {
            KEYS_USED.store(MAX_KEYS, Ordering::Release);
            return EAGAIN;
        }
        KEY_DTORS[index].store(dtor, Ordering::Release);
        // SAFETY: forwarded from the caller.
        unsafe { *key = index as c_int };
        0
    }

    #[no_mangle]
    pub extern "C" fn pthread_key_delete(_key: c_int) -> c_int {
        0
    }

    #[no_mangle]
    pub extern "C" fn pthread_setspecific(key: c_int, value: *const c_void) -> c_int {
        let me = current();
        if me.is_null() || key < 0 || key as usize >= MAX_KEYS {
            return EINVAL;
        }
        // SAFETY: our own block, and the key is in range.
        unsafe { (*me).keys[key as usize] = value as *mut c_void };
        0
    }

    #[no_mangle]
    pub extern "C" fn pthread_getspecific(key: c_int) -> *mut c_void {
        let me = current();
        if me.is_null() || key < 0 || key as usize >= MAX_KEYS {
            return ptr::null_mut();
        }
        // SAFETY: our own block, and the key is in range.
        unsafe { (*me).keys[key as usize] }
    }

    // ---- attributes ---------------------------------------------------------

    /// # Safety
    /// C ABI: `attr` points at a `pthread_attr_t`.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_attr_init(attr: *mut Attr) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe {
            (*attr).stack_pages = STACK_PAGES;
            (*attr).detached = 0;
        }
        0
    }

    /// # Safety
    /// As [`pthread_attr_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_attr_destroy(_attr: *mut Attr) -> c_int {
        0
    }

    /// # Safety
    /// As [`pthread_attr_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_attr_setstacksize(attr: *mut Attr, bytes: usize) -> c_int {
        // Rounded up to pages, and never zero: a thread with no stack is a fault at
        // its first instruction, which is a confusing way to report a bad argument.
        // SAFETY: forwarded from the caller.
        unsafe { (*attr).stack_pages = (bytes.div_ceil(4096) as u64).max(1) };
        0
    }

    /// # Safety
    /// As [`pthread_attr_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_attr_getstacksize(
        attr: *const Attr,
        bytes: *mut usize,
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { *bytes = (*attr).stack_pages as usize * 4096 };
        0
    }

    /// # Safety
    /// As [`pthread_attr_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_attr_setdetachstate(attr: *mut Attr, state: c_int) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { (*attr).detached = state };
        0
    }

    // ---- semaphores ---------------------------------------------------------

    /// # Safety
    /// C ABI: `s` points at a `sem_t`.
    #[no_mangle]
    pub unsafe extern "C" fn sem_init(s: *mut Sem, _shared: c_int, value: u32) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe {
            ptr::write_bytes(s.cast::<u8>(), 0, core::mem::size_of::<Sem>());
            (*s).count.store(value, Ordering::Release);
        }
        0
    }

    /// # Safety
    /// As [`sem_init`].
    #[no_mangle]
    pub unsafe extern "C" fn sem_destroy(_s: *mut Sem) -> c_int {
        0
    }

    /// # Safety
    /// As [`sem_init`].
    #[no_mangle]
    pub unsafe extern "C" fn sem_post(s: *mut Sem) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe {
            (*s).count.fetch_add(1, Ordering::AcqRel);
            let thread = (*s).queue.pop();
            if !thread.is_null() {
                (*thread).unpark();
            }
        }
        0
    }

    /// # Safety
    /// As [`sem_init`].
    #[no_mangle]
    pub unsafe extern "C" fn sem_wait(s: *mut Sem) -> c_int {
        let me = current();
        loop {
            // SAFETY: forwarded from the caller.
            let count = unsafe { (*s).count.load(Ordering::Acquire) };
            if count > 0 {
                // SAFETY: as above.
                let taken = unsafe {
                    (*s).count
                        .compare_exchange(count, count - 1, Ordering::AcqRel, Ordering::Relaxed)
                        .is_ok()
                };
                if taken {
                    return 0;
                }
                continue;
            }
            if me.is_null() {
                sys::yield_now();
                continue;
            }
            // SAFETY: as above; the enqueue happens before the re-check for the
            // same reason as in `Mutex::lock`.
            unsafe {
                (*s).queue.push(me);
                if (*s).count.load(Ordering::Acquire) == 0 {
                    (*me).park();
                }
            }
        }
    }

    /// # Safety
    /// As [`sem_init`].
    #[no_mangle]
    pub unsafe extern "C" fn sem_trywait(s: *mut Sem) -> c_int {
        // SAFETY: forwarded from the caller.
        let count = unsafe { (*s).count.load(Ordering::Acquire) };
        if count == 0 {
            return EAGAIN;
        }
        // SAFETY: as above.
        let taken = unsafe {
            (*s).count
                .compare_exchange(count, count - 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        };
        if taken {
            0
        } else {
            EAGAIN
        }
    }

    // ---- reader/writer locks ------------------------------------------------

    /// # Safety
    /// C ABI: `l` points at a `pthread_rwlock_t`. All-zero is unlocked, so the
    /// static initialiser needs no constructor.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_rwlock_init(l: *mut RwLock, _attr: *const c_void) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { ptr::write_bytes(l.cast::<u8>(), 0, core::mem::size_of::<RwLock>()) };
        0
    }

    /// # Safety
    /// As [`pthread_rwlock_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_rwlock_destroy(_l: *mut RwLock) -> c_int {
        0
    }

    /// # Safety
    /// As [`pthread_rwlock_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_rwlock_rdlock(l: *mut RwLock) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { (*l).read_lock() };
        0
    }

    /// # Safety
    /// As [`pthread_rwlock_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_rwlock_wrlock(l: *mut RwLock) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { (*l).write_lock() };
        0
    }

    /// # Safety
    /// As [`pthread_rwlock_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_rwlock_unlock(l: *mut RwLock) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { (*l).unlock() };
        0
    }

    // ---- attributes nobody can honour ---------------------------------------
    //
    // These exist because Qt links against them, and each returns the truth about a
    // system with one scheduling policy, no signals and no cancellation. A stub
    // that claimed success would let a caller believe it had set a priority.

    /// # Safety
    /// C ABI: `attr` points at a `pthread_condattr_t`; nothing is stored.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_condattr_init(_attr: *mut c_void) -> c_int {
        0
    }

    /// # Safety
    /// As [`pthread_condattr_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_condattr_destroy(_attr: *mut c_void) -> c_int {
        0
    }

    /// # Safety
    /// As [`pthread_condattr_init`]. There is one clock and it is monotonic, so
    /// selecting it succeeds and selecting anything else is refused rather than
    /// silently ignored.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_condattr_setclock(_attr: *mut c_void, clock: c_int) -> c_int {
        // CLOCK_REALTIME (0) and CLOCK_MONOTONIC (1) both land on the same counter.
        if clock == 0 || clock == 1 {
            0
        } else {
            EINVAL
        }
    }

    /// # Safety
    /// As [`pthread_cond_timedwait`]; the clock id is accepted and ignored because
    /// there is only one clock.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_cond_clockwait(
        c: *mut Cond,
        m: *mut Mutex,
        _clock: c_int,
        abstime: *const crate::time::Timespec,
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { pthread_cond_timedwait(c, m, abstime) }
    }

    /// # Safety
    /// As [`pthread_join`]; the deadline is ignored — a join here waits as long as
    /// the thread does, and reporting a timeout that cannot happen would be worse
    /// than not offering one.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_clockjoin_np(
        id: usize,
        retval: *mut *mut c_void,
        _clock: c_int,
        _abstime: *const crate::time::Timespec,
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { pthread_join(id, retval) }
    }

    /// # Safety
    /// As [`pthread_attr_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_attr_setschedpolicy(_attr: *mut Attr, policy: c_int) -> c_int {
        // Policy 0 (SCHED_OTHER) is the only one this scheduler implements.
        if policy == 0 {
            0
        } else {
            EINVAL
        }
    }

    /// # Safety
    /// As [`pthread_attr_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_attr_getschedpolicy(
        _attr: *const Attr,
        policy: *mut c_int,
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { *policy = 0 };
        0
    }

    /// # Safety
    /// As [`pthread_attr_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_attr_setschedparam(
        _attr: *mut Attr,
        _param: *const c_int,
    ) -> c_int {
        0
    }

    /// # Safety
    /// As [`pthread_attr_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_attr_setinheritsched(
        _attr: *mut Attr,
        _inherit: c_int,
    ) -> c_int {
        0
    }

    /// # Safety
    /// C ABI: `attr` receives this thread's attributes.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_getattr_np(_id: usize, attr: *mut Attr) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { pthread_attr_init(attr) }
    }

    /// # Safety
    /// C ABI: `base` and `size` receive the thread's stack.
    ///
    /// Both come back zero: the kernel maps the stack and never tells the thread
    /// where it is. A guessed answer here would be used by a garbage collector or a
    /// stack-overflow check, which are exactly the callers that must not be lied to.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_attr_getstack(
        _attr: *const Attr,
        base: *mut *mut c_void,
        size: *mut usize,
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe {
            if !base.is_null() {
                *base = ptr::null_mut();
            }
            if !size.is_null() {
                *size = 0;
            }
        }
        ENOSYS
    }

    /// # Safety
    /// C ABI: `name` is a buffer of `len` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_getname_np(_id: usize, name: *mut u8, len: usize) -> c_int {
        if len > 0 {
            // SAFETY: forwarded from the caller.
            unsafe { *name = 0 };
        }
        0
    }

    /// Cancellation does not exist here, and saying so is the point: a thread that
    /// believed it had been cancelled would wait forever for a stop that never
    /// comes.
    #[no_mangle]
    pub extern "C" fn pthread_cancel(_id: usize) -> c_int {
        ENOSYS
    }

    /// # Safety
    /// C ABI: `old` may be null.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_setcancelstate(_state: c_int, old: *mut c_int) -> c_int {
        if !old.is_null() {
            // PTHREAD_CANCEL_DISABLE — which is the truth, permanently.
            // SAFETY: forwarded from the caller.
            unsafe { *old = 1 };
        }
        0
    }

    #[no_mangle]
    pub extern "C" fn pthread_testcancel() {}

    // `pthread_sigmask` used to live here and ignored everything it was given. It
    // is in `crate::proc` now, over the same one process-wide mask `sigprocmask`
    // reads and writes — there is one mask because there is one signal state, and a
    // program that sets a mask and reads it back is entitled to what it set.

    /// # Safety
    /// C ABI: `set` receives the affinity mask.
    #[no_mangle]
    pub unsafe extern "C" fn sched_getaffinity(
        _pid: c_int,
        len: usize,
        set: *mut u8,
    ) -> c_int {
        if len == 0 || set.is_null() {
            return EINVAL;
        }
        // One CPU, bit 0. Threads run wherever the scheduler puts them; there is no
        // affinity to report and pretending to more CPUs would make a thread pool
        // size itself wrongly.
        // SAFETY: forwarded from the caller.
        unsafe {
            ptr::write_bytes(set, 0, len);
            *set = 1;
        }
        0
    }

    /// # Safety
    /// C ABI: `set` is a mask of `len` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn __sched_cpucount(len: usize, set: *const u8) -> c_int {
        let mut count = 0;
        for i in 0..len {
            // SAFETY: forwarded from the caller.
            count += unsafe { (*set.add(i)).count_ones() } as c_int;
        }
        count
    }

    // Named semaphores need a namespace to name them in, and this system's only
    // namespace is a read-only archive. They exist so a program that references
    // them links, and they refuse so a program that *uses* them finds out here
    // rather than three layers up. `sem_open` returns `SEM_FAILED`, which is the
    // documented failure value and is what a caller checks for.
    /// # Safety
    /// C ABI: variadic in POSIX; the extra arguments are never read.
    #[no_mangle]
    pub unsafe extern "C" fn sem_open(_name: *const c_int, _flags: c_int) -> *mut Sem {
        usize::MAX as *mut Sem
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn sem_close(_s: *mut Sem) -> c_int {
        ENOSYS
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn sem_unlink(_name: *const c_int) -> c_int {
        ENOSYS
    }

    // ---- scheduling ---------------------------------------------------------

    #[no_mangle]
    pub extern "C" fn sched_yield() -> c_int {
        sys::yield_now();
        0
    }

    /// The one scheduling policy this system has. Reported honestly rather than
    /// pretending to a range of priorities the kernel does not implement: a caller
    /// that gets 0..0 knows there is nothing to choose.
    #[no_mangle]
    pub extern "C" fn sched_get_priority_min(_policy: c_int) -> c_int {
        0
    }

    #[no_mangle]
    pub extern "C" fn sched_get_priority_max(_policy: c_int) -> c_int {
        0
    }

    /// # Safety
    /// C ABI: the pointers are ignored.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_getschedparam(
        _id: usize,
        policy: *mut c_int,
        param: *mut c_int,
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe {
            if !policy.is_null() {
                *policy = 0;
            }
            if !param.is_null() {
                *param = 0;
            }
        }
        0
    }

    #[no_mangle]
    pub extern "C" fn pthread_setschedparam(_id: usize, _policy: c_int, _param: *const c_int) -> c_int {
        0
    }

    /// Live thread accounting, for the same reason the heap exposes its own: a
    /// program with no visibility into its runtime cannot tell a leak from a
    /// design.
    #[no_mangle]
    pub extern "C" fn staros_threads_live() -> usize {
        SLOTS.iter().filter(|s| s.load(Ordering::Acquire) == 1).count()
    }

    /// The thread pointer, so a program can prove its TLS is really per-thread
    /// rather than one block everybody shares.
    #[no_mangle]
    pub extern "C" fn staros_thread_pointer() -> usize {
        super::thread_pointer() as usize
    }
}
