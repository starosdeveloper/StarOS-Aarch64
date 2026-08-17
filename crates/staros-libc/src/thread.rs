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

/// The most a thread may ask for, which is `MAX_THREAD_STACK_PAGES` in the kernel's
/// `SpawnThread`. Another copy of a kernel constant, for the reason given at
/// [`MAIN_STACK_SIZE`].
///
/// 8 MiB, which is Qt's number: `QQmlThreadPrivate` asks for exactly that because
/// QML's parser and code generator have recursion limits calibrated to it. This is
/// the ceiling a caller may ask for, not what a thread gets — [`STACK_PAGES`] is the
/// default, and these pages are mapped eagerly.
const MAX_THREAD_STACK_PAGES: u64 = 2048;

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
    /// This thread's stack, as `[top - size, top)`. Written once by the thread
    /// itself before it runs anything else, read by [`pthread_attr_getstack`].
    ///
    /// Atomic because a thread may ask about another thread's stack, and the answer
    /// is written by that other thread. Zero means "not yet recorded", which is a
    /// distinguishable answer rather than a wrong one.
    stack_top: AtomicUsize,
    stack_size: AtomicUsize,
    /// Destructors for this thread's `thread_local` objects, most recent last.
    ///
    /// Plain fields rather than atomics: only the owning thread ever touches them —
    /// it registers them as its `thread_local`s are constructed and runs them on its
    /// way out. Nothing else has any business in this list.
    tls_dtors: [TlsDtor; MAX_TLS_DTORS],
    tls_dtor_count: usize,
}

/// One `thread_local` destructor and the object it belongs to.
#[derive(Clone, Copy)]
struct TlsDtor {
    function: Option<unsafe extern "C" fn(*mut c_void)>,
    object: *mut c_void,
}

/// How many `thread_local` objects one thread may have.
///
/// Fixed, for the same reason the process-wide `__cxa_atexit` table is fixed
/// (`crates/staros-libc/src/cxx.rs`): registration happens *while* those objects are
/// being constructed, and an allocation here would run the allocator from inside a
/// constructor that may be the allocator's own.
///
/// Thirty-two. Qt's per-thread state is a handful of objects — `QThreadData`, the
/// event dispatcher's, the current-thread pointer — and a program that wants more
/// gets a refusal from `__cxa_thread_atexit` rather than a destructor that silently
/// never runs.
const MAX_TLS_DTORS: usize = 32;

/// Where the kernel puts the main thread's stack, and how far it lets it grow.
///
/// These are `arch_aarch64::addrspace::USER_STACK_TOP` and `USER_STACK_MAX_PAGES`,
/// and they are written here rather than imported because this library does not
/// depend on the kernel's crates — the ABI is what the two share. A copy of a
/// constant is a seam, so it is named on both sides: change one and the other is a
/// grep away.
///
/// The main thread's stack is not mapped up front. One page exists at start-up and
/// the rest arrive on the fault, up to the limit; the *region* is the whole extent,
/// which is what a caller asking where its stack is wants to know.
const MAIN_STACK_TOP: usize = 0x8_0000_0000;
pub(crate) const MAIN_STACK_SIZE: usize = 256 * 4096;

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
        unsafe {
            (*main_thread).stack_top.store(MAIN_STACK_TOP, Ordering::Release);
            (*main_thread).stack_size.store(MAIN_STACK_SIZE, Ordering::Release);
            install(main_thread);
        }
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
            stack_top: AtomicUsize::new(0),
            stack_size: AtomicUsize::new(0),
            tls_dtors: [TlsDtor { function: None, object: ptr::null_mut() }; MAX_TLS_DTORS],
            tls_dtor_count: 0,
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

/// Run everything this thread has to run before it stops existing.
///
/// Two lists, in the order the standards give them: the destructors of the thread's
/// `thread_local` objects first, then the destructors registered with
/// `pthread_key_create` for whichever keys this thread set a value on. C++ specifies
/// the first ordering; POSIX specifies the second, including the re-scan (a
/// destructor may set the key again, and the value must be destroyed too).
///
/// This used not to happen at all, and the shape of that failure is worth keeping:
/// `__cxa_thread_atexit` put every `thread_local` destructor on the *process*
/// `atexit` list, and `pthread_key_create` accepted destructors and never ran them.
/// Both were written down as known limitations. Then a QML program stopped in
/// `QThread::wait`, because `QThreadPrivate::cleanup` — which is what calls
/// `wakeAll()` on the condition `wait` is sleeping on — runs from the destructor of
/// a `thread_local`. A destructor that runs at process exit instead of at thread
/// exit is not late here. It is never, because the process was waiting for it.
///
/// # Safety
/// Called on the thread that is ending, once, before anything else observes it as
/// finished.
unsafe fn run_thread_exit(thread: *mut Thread) {
    if thread.is_null() {
        return;
    }
    // Most recent first, and the count is cleared as we go: a destructor that
    // constructs another `thread_local` registers it, and running that one too is
    // what the loop's re-read of the count achieves.
    loop {
        // SAFETY: our own block, and only this thread touches these fields.
        let entry = unsafe {
            let count = (*thread).tls_dtor_count;
            if count == 0 {
                break;
            }
            (*thread).tls_dtor_count = count - 1;
            (*thread).tls_dtors[count - 1]
        };
        if let Some(function) = entry.function {
            // SAFETY: the pair came from a `__cxa_thread_atexit` call.
            unsafe { function(entry.object) };
        }
    }

    // POSIX's key destructors, and its rule for them: repeat the sweep until no key
    // has a value left or the round limit is reached, because a destructor is
    // allowed to set its own key again.
    const ROUNDS: usize = 4;
    for _ in 0..ROUNDS {
        let mut any = false;
        for key in 0..MAX_KEYS {
            // SAFETY: our own block.
            let value = unsafe { (*thread).keys[key] };
            if value.is_null() {
                continue;
            }
            let dtor = KEY_DTORS[key].load(Ordering::Acquire);
            // Cleared before the call, as POSIX requires: the destructor sees a key
            // with no value, so a destructor that reads its own key does not get the
            // object it is destroying.
            // SAFETY: our own block.
            unsafe { (*thread).keys[key] = ptr::null_mut() };
            if dtor.is_null() {
                continue;
            }
            any = true;
            // SAFETY: the pointer came from `pthread_key_create`, whose declared
            // type is this.
            let dtor: unsafe extern "C" fn(*mut c_void) =
                unsafe { core::mem::transmute(dtor) };
            // SAFETY: as registered.
            unsafe { dtor(value) };
        }
        if !any {
            break;
        }
    }
}

/// Run the current thread's exit lists. Called from the process exit path for
/// `main`, which is a thread like any other and whose `thread_local`s would
/// otherwise never be destroyed at all.
///
/// # Safety
/// Called once, on the way out.
pub(crate) unsafe fn run_current_thread_exit() {
    // SAFETY: forwarded; `current()` is this thread's own block.
    unsafe { run_thread_exit(current()) };
}

/// The current stack pointer, or zero where there is no such register to name.
///
/// Zero rather than a plausible address off the host build: this is used to work out
/// where a thread's stack is, and the host tests neither have this system's stack
/// layout nor ask about it.
fn stack_pointer() -> usize {
    #[cfg(target_arch = "aarch64")]
    {
        let sp: usize;
        // SAFETY: reading the stack pointer has no side effects.
        unsafe { core::arch::asm!("mov {sp}, sp", sp = out(reg) sp, options(nomem, nostack)) };
        sp
    }
    #[cfg(not(target_arch = "aarch64"))]
    0
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

/// Print the call chain leading here, as raw return addresses.
///
/// Temporary, and switched on only by `STAROS_PARK_TRACE`. A park that never returns
/// is indistinguishable from every other park from the outside — the kernel can say
/// a task is blocked and even on which endpoint, but a notification wait has no
/// endpoint and no name. These addresses do: `llvm-addr2line` against the unstripped
/// executable turns them into the Qt frames that asked to sleep.
///
/// Walks the frame chain by hand rather than using a library, because there is no
/// unwinder here — exceptions abort, and `.eh_frame` is discarded from the image. The
/// AArch64 ABI puts the caller's frame pointer at `[x29]` and its return address at
/// `[x29, #8]`, which is enough for any function that keeps a frame. A leaf that does
/// not simply does not appear.
#[cfg(feature = "park-trace")]
pub(crate) fn park_trace(site: &str) {
    struct Buf {
        bytes: [u8; 192],
        len: usize,
    }
    impl Buf {
        fn put(&mut self, bytes: &[u8]) {
            for &b in bytes {
                if self.len < self.bytes.len() {
                    self.bytes[self.len] = b;
                    self.len += 1;
                }
            }
        }
    }
    let mut buf = Buf { bytes: [0; 192], len: 0 };
    buf.put(b"[park] ");
    buf.put(site.as_bytes());

    let mut fp: usize;
    // SAFETY: reading the frame pointer register has no effect.
    unsafe { core::arch::asm!("mov {}, x29", out(reg) fp, options(nomem, nostack)) };
    for _ in 0..6 {
        // A frame pointer that is null, misaligned or below the last one has left
        // the chain. Following it would read whatever happens to be there and print
        // an address that means nothing.
        if fp == 0 || fp & 7 != 0 {
            break;
        }
        // SAFETY: `fp` points at a frame record while the chain holds; the checks
        // above reject the values that say it no longer does.
        let (next, lr) =
            unsafe { ((fp as *const usize).read(), (fp as *const usize).add(1).read()) };
        let mut hex = [0u8; 19];
        hex[0] = b' ';
        hex[1] = b'0';
        hex[2] = b'x';
        for i in 0..16 {
            let nibble = ((lr >> (60 - i * 4)) & 0xf) as u8;
            hex[3 + i] = if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 };
        }
        buf.put(&hex);
        if next <= fp {
            break;
        }
        fp = next;
    }
    buf.put(b"\n");
    sys::debug_write(&buf.bytes[..buf.len]);
}

impl Thread {
    /// Sleep until somebody unparks us.
    fn park(&self) {
        sys::wait(self.parker);
    }

    /// As [`park`], with a note on the console saying who asked. See [`park_trace`].
    fn park_at(&self, site: &str) {
        #[cfg(feature = "park-trace")]
        park_trace(site);
        #[cfg(not(feature = "park-trace"))]
        let _ = site;
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
            unsafe { (*me).park_at("mutex") };
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

/// `sem_t` is 32 bytes in <semaphore.h>, and the two halves have to agree.
///
/// The size is the ABI, not an implementation detail: callers embed a `sem_t` in
/// their own structures and this library writes through the pointer they hand back.
/// A disagreement would not be caught by any test — `sem_init` would zero the right
/// number of bytes according to *this* side, and the caller's neighbouring field
/// would be the one that changed.
const _: () = {
    assert!(core::mem::size_of::<Sem>() == 32);
    assert!(core::mem::align_of::<Sem>() == 8);
};

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
                unsafe { (*me).park_at("rwlock-read") };
            }
        }
    }

    /// Take a read lock if no writer holds or is taking it. Never blocks.
    ///
    /// One attempt and no retry: `try` means "tell me whether it was free", and a
    /// version that looped on the writer-took-it-in-the-window case would block for
    /// as long as writers kept arriving — which is the one thing its caller chose it
    /// to avoid.
    fn try_read_lock(&self) -> bool {
        if self.writer.load(Ordering::Acquire) != 0 {
            return false;
        }
        self.readers.fetch_add(1, Ordering::AcqRel);
        if self.writer.load(Ordering::Acquire) == 0 {
            return true;
        }
        self.readers.fetch_sub(1, Ordering::AcqRel);
        false
    }

    /// Take the write lock if nothing holds it. Never blocks.
    fn try_write_lock(&self) -> bool {
        if self
            .writer
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        if self.readers.load(Ordering::Acquire) == 0 {
            self.owner.store(current(), Ordering::Release);
            return true;
        }
        // Readers are still inside. Give the flag back and wake whoever was waiting
        // on it — leaving it set would lock out every reader for ever on behalf of a
        // writer that gave up.
        self.writer.store(0, Ordering::Release);
        Queue::wake_all(self.queue.take());
        false
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
                unsafe { (*me).park_at("rwlock-write") };
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

    // Where this thread's stack is, recorded by the only party that can know.
    //
    // The kernel maps the stack and sets `sp` to the top of it, and the top is page
    // aligned — `sched::spawn_thread` reserves whole pages and points `sp` at
    // `base + pages * 4096`. The parent is never told the address, so the child
    // recovers it from the one place it appears: its own stack pointer, rounded up
    // to the page boundary it started on. That is a derivation from two facts the
    // kernel states, not an estimate: the only thing between the top and `sp` here
    // is this function's prologue, which is a few dozen bytes.
    //
    // `pthread_create` has already written the size.
    let sp = stack_pointer();
    if sp != 0 {
        // SAFETY: `arg` is the control block this thread was created with.
        unsafe { (*thread).stack_top.store((sp + 0xfff) & !0xfff, Ordering::Release) };
    }

    // SAFETY: `arg` is the control block this thread was created with; the kernel
    // already installed our thread pointer, so `current()` agrees with it.
    let (start, argument) = unsafe { ((*thread).start, (*thread).arg) };
    let retval = match start {
        Some(f) => f(argument),
        None => ptr::null_mut(),
    };
    // Before anything observes this thread as finished. A joiner that woke first
    // would be entitled to reuse everything the destructors below are still using.
    // SAFETY: our own block, and we are the thread it describes.
    unsafe { run_thread_exit(thread) };
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
        Queue, RwLock, Sem, Thread, KEYS_USED, KEY_DTORS, MAX_KEYS, MAX_THREAD_STACK_PAGES, SLOTS,
        STACK_PAGES,
    };

    /// `EAGAIN`, the errno POSIX gives for "no resources for another thread".
    const EAGAIN: c_int = 11;
    /// `ETIMEDOUT`, for the timed waits.
    const ETIMEDOUT: c_int = 110;
    /// `EINVAL`.
    const EINVAL: c_int = 22;
    /// No such thread — what `pthread_getattr_np` answers when it is asked about one
    /// that has not started, which is distinguishable from a bad argument.
    const ESRCH: c_int = 3;
    /// `EBUSY`, for `pthread_mutex_trylock`.
    const EBUSY: c_int = 16;
    /// `ENOSYS`, for the things this system genuinely does not have.
    const ENOSYS: c_int = 38;

    /// `pthread_attr_t` as this library defines it: a stack size, a detach flag, and
    /// — once [`pthread_getattr_np`] has filled it in — where a *running* thread's
    /// stack actually is. Four words, matching `pthread.h`.
    #[repr(C)]
    pub struct Attr {
        stack_pages: u64,
        detached: c_int,
        /// Low address of the stack region, or zero when this attribute block
        /// describes a thread that does not exist yet. Zero is why
        /// `pthread_attr_getstack` can tell "nobody asked about a real thread" from
        /// "here is where it is".
        stack_base: u64,
        _pad: [u64; 1],
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

        // The size is known here and the address is not — the child writes that in
        // `thread_entry`, from the one register it appears in.
        // SAFETY: nothing else can see the block yet.
        unsafe {
            (*block)
                .stack_size
                .store(pages as usize * 4096, Ordering::Release);
        }

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
                (*me).park_at("join");
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
            // The same order as [`thread_entry`]: the exit lists run before anyone
            // is told this thread has finished.
            // SAFETY: our own block, and we are the thread it describes.
            unsafe { super::run_thread_exit(me) };
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

    /// A mutex attribute object.
    ///
    /// One word, and the only thing anybody sets in it is the type. `PTHREAD_MUTEX_
    /// RECURSIVE` is the one that matters — libstdc++'s `std::recursive_mutex` asks
    /// for it — and this system's mutex is not recursive, so asking is refused with
    /// `EINVAL` rather than accepted and forgotten. A recursive lock that silently
    /// is not one deadlocks a thread against itself, which looks like a hang in
    /// whatever the second lock was for.
    #[repr(C)]
    pub struct MutexAttr {
        kind: c_int,
    }

    /// `PTHREAD_MUTEX_NORMAL`, the only type there is here.
    const PTHREAD_MUTEX_NORMAL: c_int = 0;

    /// # Safety
    /// C ABI: `a` points at one writable `pthread_mutexattr_t`.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_mutexattr_init(a: *mut MutexAttr) -> c_int {
        if a.is_null() {
            return EINVAL;
        }
        // SAFETY: the caller passes a writable attribute object.
        unsafe { (*a).kind = PTHREAD_MUTEX_NORMAL };
        0
    }

    /// # Safety
    /// As [`pthread_mutexattr_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_mutexattr_destroy(a: *mut MutexAttr) -> c_int {
        if a.is_null() {
            EINVAL
        } else {
            0
        }
    }

    /// # Safety
    /// As [`pthread_mutexattr_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_mutexattr_settype(a: *mut MutexAttr, kind: c_int) -> c_int {
        if a.is_null() {
            return EINVAL;
        }
        if kind != PTHREAD_MUTEX_NORMAL {
            // Refused, not ignored. See the note on `MutexAttr`.
            return EINVAL;
        }
        // SAFETY: the caller passes a writable attribute object.
        unsafe { (*a).kind = kind };
        0
    }

    /// `pthread_atfork`: run handlers around `fork`.
    ///
    /// There is no `fork` here — it returns `ENOSYS` — so a handler registered
    /// against it can never run. Accepting the registration would be a promise this
    /// system cannot keep; `ENOSYS` says so at the moment it is made, which is the
    /// only moment a caller can do anything about it.
    #[no_mangle]
    pub extern "C" fn pthread_atfork(
        _prepare: Option<extern "C" fn()>,
        _parent: Option<extern "C" fn()>,
        _child: Option<extern "C" fn()>,
    ) -> c_int {
        38 // ENOSYS
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

    /// `pthread_mutex_timedlock`: try until an absolute deadline.
    ///
    /// Spin-and-yield rather than parking with a timeout. The mutex's wait queue has
    /// no notion of a deadline — a parked thread is woken by the unlock and by
    /// nothing else — and adding one would mean a timer per waiter for a call that,
    /// so far, nothing makes. What is here is honest and correct; it burns a
    /// timeslice while it waits, and that is written down rather than hidden.
    ///
    /// # Safety
    /// C ABI: `m` is an initialised mutex; `deadline` is one `struct timespec`.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_mutex_timedlock(
        m: *mut Mutex,
        deadline: *const crate::time::Timespec,
    ) -> c_int {
        if deadline.is_null() {
            return EINVAL;
        }
        // SAFETY: the caller passes a readable `timespec`.
        let want = unsafe { crate::time::join((*deadline).tv_sec, (*deadline).tv_nsec) };
        loop {
            // SAFETY: forwarded from the caller.
            if unsafe { (*m).try_lock() } {
                return 0;
            }
            match sys::clock_now() {
                Some(now) if now >= want => return 110, // ETIMEDOUT
                // No clock at all: the deadline can never be observed to pass, and a
                // loop that waited for it would never end. Refusing is the answer a
                // caller can act on.
                None => return EINVAL,
                _ => sys::yield_now(),
            }
        }
    }

    /// `pthread_mutex_clocklock`: the same, with the clock named. There is one
    /// clock here (see [`crate::time`]), so the argument is accepted and the answer
    /// is the same — which is worth saying, because a caller passing
    /// `CLOCK_REALTIME` and expecting wall-clock behaviour will get monotonic.
    ///
    /// # Safety
    /// As [`pthread_mutex_timedlock`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_mutex_clocklock(
        m: *mut Mutex,
        _clock: c_int,
        deadline: *const crate::time::Timespec,
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { pthread_mutex_timedlock(m, deadline) }
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
            (*me).park_at("cond");
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

    /// Register a destructor for one of this thread's `thread_local` objects.
    ///
    /// The compiler emits a call to this for every `thread_local` with a non-trivial
    /// destructor, right after constructing it. The list is per thread and runs when
    /// the thread ends — see [`run_thread_exit`](super::run_thread_exit) for what
    /// that used to do instead, and what it cost.
    ///
    /// Both spellings are defined because both are emitted: glibc's header declares
    /// `__cxa_thread_atexit_impl` and libstdc++ calls `__cxa_thread_atexit`, and
    /// which one appears depends on how the translation unit was compiled.
    ///
    /// # Safety
    /// C ABI: `destructor` is called with `object` when this thread ends.
    #[no_mangle]
    pub unsafe extern "C" fn __cxa_thread_atexit(
        destructor: Option<unsafe extern "C" fn(*mut c_void)>,
        object: *mut c_void,
        _dso: *mut c_void,
    ) -> c_int {
        let me = current();
        if me.is_null() {
            // Before `thread::init` ran, which is before `main`. Nothing has a
            // thread block yet, and a `thread_local` constructed this early belongs
            // to the process for as long as it exists.
            return -1;
        }
        // SAFETY: our own block; only this thread touches these fields.
        unsafe {
            let count = (*me).tls_dtor_count;
            if count >= super::MAX_TLS_DTORS {
                // Refused rather than dropped. The caller turns a refusal into a
                // failed construction; a destructor quietly never registered is a
                // leak nobody can see.
                return -1;
            }
            (*me).tls_dtors[count] = super::TlsDtor { function: destructor, object };
            (*me).tls_dtor_count = count + 1;
        }
        0
    }

    /// # Safety
    /// As [`__cxa_thread_atexit`].
    #[no_mangle]
    pub unsafe extern "C" fn __cxa_thread_atexit_impl(
        destructor: Option<unsafe extern "C" fn(*mut c_void)>,
        object: *mut c_void,
        dso: *mut c_void,
    ) -> c_int {
        // SAFETY: forwarded.
        unsafe { __cxa_thread_atexit(destructor, object, dso) }
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
            // Fresh attributes describe a thread that does not exist yet, so there
            // is no stack to point at. `pthread_getattr_np` is what fills this in.
            (*attr).stack_base = 0;
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
        let pages = (bytes.div_ceil(4096) as u64).max(1);
        // Refused here rather than at `pthread_create`, because this is where the
        // caller named the number. A size past the kernel's ceiling makes
        // `SpawnThread` answer `InvalidArgument`, which `pthread_create` can only
        // report as `EAGAIN` — "try again later" for a request that will never
        // succeed. `EINVAL` at the point of the argument is the truth.
        if pages > MAX_THREAD_STACK_PAGES {
            return EINVAL;
        }
        // SAFETY: forwarded from the caller.
        unsafe { (*attr).stack_pages = pages };
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
    //
    // These report failure differently from everything above them, and the
    // difference is POSIX's rather than this library's: the `pthread_*` functions
    // return the error number, while `sem_*` return -1 and set `errno`, like an
    // ordinary syscall wrapper. They sit in the same module and look like siblings,
    // which is exactly why it is written down here.
    //
    // `sem_trywait` returned `EAGAIN` — 11, a positive number — until this was
    // noticed. Every caller checking `== -1` read that as success and went on to use
    // a semaphore it had not acquired. Nothing in this tree called it, which is why
    // it survived; Qt calls `sem_timedwait`, which is what brought the whole family
    // under review.

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
                    (*me).park_at("sem");
                }
            }
        }
    }

    /// `sem_timedwait`: [`sem_wait`] with a deadline.
    ///
    /// It spins with a yield rather than parking, and that is the same limitation
    /// [`pthread_mutex_timedlock`] has for the same reason: the kernel's parkers
    /// have no notion of a deadline, so a parked thread has nothing to wake it when
    /// the time arrives. A waiter therefore burns its timeslice until the semaphore
    /// is posted or the deadline passes. Giving the kernel timed parking is a change
    /// to the kernel, not to this function.
    ///
    /// With no clock at all the deadline can never be observed to pass and a waiting
    /// loop would never end, so that case refuses with `EINVAL` — an answer a caller
    /// can act on — rather than hanging.
    ///
    /// # Safety
    /// As [`sem_init`]; `deadline` is null or a readable `timespec`.
    #[no_mangle]
    pub unsafe extern "C" fn sem_timedwait(
        s: *mut Sem,
        deadline: *const crate::time::Timespec,
    ) -> c_int {
        if deadline.is_null() {
            return crate::fail(EINVAL, -1);
        }
        // SAFETY: the caller passes a readable `timespec`.
        let want = unsafe { crate::time::join((*deadline).tv_sec, (*deadline).tv_nsec) };
        loop {
            // SAFETY: forwarded from the caller.
            if unsafe { sem_trywait(s) } == 0 {
                return 0;
            }
            match sys::clock_now() {
                Some(now) if now >= want => return crate::fail(110, -1), // ETIMEDOUT
                None => return crate::fail(EINVAL, -1),
                _ => sys::yield_now(),
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
            return crate::fail(EAGAIN, -1);
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
            // Lost the race to another thread, which is indistinguishable from
            // having found it at zero — and `EAGAIN` is what POSIX says for both.
            crate::fail(EAGAIN, -1)
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
    pub unsafe extern "C" fn pthread_rwlock_tryrdlock(l: *mut RwLock) -> c_int {
        // SAFETY: forwarded from the caller.
        if unsafe { (*l).try_read_lock() } {
            0
        } else {
            EBUSY
        }
    }

    /// # Safety
    /// As [`pthread_rwlock_init`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_rwlock_trywrlock(l: *mut RwLock) -> c_int {
        // SAFETY: forwarded from the caller.
        if unsafe { (*l).try_write_lock() } {
            0
        } else {
            EBUSY
        }
    }

    /// The timed read and write locks, and their `clock`-taking twins.
    ///
    /// `std::shared_mutex` references all four from libstdc++'s header, so a
    /// translation unit that merely includes `<shared_mutex>` needs them — Qt does.
    ///
    /// Spin and yield, as [`pthread_mutex_timedlock`] does: the wait queue has no
    /// deadline, and giving it one is a change to the kernel's parkers rather than
    /// to this library. A waiter burns its timeslice, which is the honest cost and
    /// is said here rather than discovered in a profile.
    ///
    /// # Safety
    /// C ABI: `l` is an initialised lock; `deadline` is one `struct timespec`.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_rwlock_timedrdlock(
        l: *mut RwLock,
        deadline: *const crate::time::Timespec,
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { timed_rwlock(l, deadline, false) }
    }

    /// # Safety
    /// As [`pthread_rwlock_timedrdlock`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_rwlock_timedwrlock(
        l: *mut RwLock,
        deadline: *const crate::time::Timespec,
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { timed_rwlock(l, deadline, true) }
    }

    /// # Safety
    /// As [`pthread_rwlock_timedrdlock`]. The clock is accepted and ignored:
    /// there is one clock here, and `docs/LIBC-CONTRACT.md` says which.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_rwlock_clockrdlock(
        l: *mut RwLock,
        _clock: c_int,
        deadline: *const crate::time::Timespec,
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { timed_rwlock(l, deadline, false) }
    }

    /// # Safety
    /// As [`pthread_rwlock_clockrdlock`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_rwlock_clockwrlock(
        l: *mut RwLock,
        _clock: c_int,
        deadline: *const crate::time::Timespec,
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { timed_rwlock(l, deadline, true) }
    }

    /// The body all four share.
    ///
    /// # Safety
    /// C ABI: as the four above.
    unsafe fn timed_rwlock(
        l: *mut RwLock,
        deadline: *const crate::time::Timespec,
        write: bool,
    ) -> c_int {
        if deadline.is_null() {
            return EINVAL;
        }
        // SAFETY: the caller passes a readable `timespec`.
        let want = unsafe { crate::time::join((*deadline).tv_sec, (*deadline).tv_nsec) };
        loop {
            // SAFETY: forwarded from the caller.
            let taken = unsafe {
                if write {
                    (*l).try_write_lock()
                } else {
                    (*l).try_read_lock()
                }
            };
            if taken {
                return 0;
            }
            match sys::clock_now() {
                Some(now) if now >= want => return 110, // ETIMEDOUT
                // No clock: the deadline can never be observed to pass, so a loop
                // waiting for it would never end. Refusing is what a caller can act
                // on.
                None => return EINVAL,
                _ => sys::yield_now(),
            }
        }
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

    /// The attributes of a thread that is *running*, which is a different question
    /// from the attributes a thread would be created with.
    ///
    /// # Safety
    /// C ABI: `id` names a live thread, or is this one; `attr` receives its
    /// attributes.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_getattr_np(id: usize, attr: *mut Attr) -> c_int {
        if attr.is_null() {
            return EINVAL;
        }
        // SAFETY: forwarded from the caller.
        unsafe { pthread_attr_init(attr) };

        let thread = if id == 0 { super::current() } else { id as *mut Thread };
        if thread.is_null() {
            return ESRCH;
        }
        // SAFETY: `thread` is a live control block; both fields are atomics written
        // by the thread they describe.
        unsafe {
            let top = (*thread).stack_top.load(Ordering::Acquire);
            let size = (*thread).stack_size.load(Ordering::Acquire);
            if top == 0 || size == 0 {
                // The thread has not recorded its stack yet, which for a thread
                // that exists means it has not reached its first instruction.
                // Saying so beats handing back the defaults as if they were facts.
                return ESRCH;
            }
            (*attr).stack_base = (top - size) as u64;
            (*attr).stack_pages = (size / 4096) as u64;
        }
        0
    }

    /// # Safety
    /// C ABI: `base` and `size` receive the thread's stack.
    ///
    /// This used to return `ENOSYS` with both outputs zeroed, on the grounds that
    /// the kernel maps the stack and never tells the thread where it is, and that a
    /// guessed answer would be used by exactly the callers who must not be lied to —
    /// garbage collectors and stack-overflow checks.
    ///
    /// The second half of that was right and the first half was not. V4, the
    /// JavaScript engine inside QML, is precisely such a caller: `stackProperties()`
    /// in `qv4stacklimits.cpp` ends with `qFatal("Cannot find stack base")` when this
    /// refuses, so the refusal was not a safe answer but a QML program that aborted
    /// before its first line ran.
    ///
    /// And the answer was available all along. The main thread's stack is the
    /// kernel's own `[USER_STACK_TOP - USER_STACK_MAX_PAGES * 4096, USER_STACK_TOP)`,
    /// which is a constant of this ABI; a spawned thread's is `pages` pages ending at
    /// the `sp` the kernel started it with, which the thread records in
    /// `thread_entry`. Neither is a guess.
    #[no_mangle]
    pub unsafe extern "C" fn pthread_attr_getstack(
        attr: *const Attr,
        base: *mut *mut c_void,
        size: *mut usize,
    ) -> c_int {
        if attr.is_null() {
            return EINVAL;
        }
        // SAFETY: forwarded from the caller.
        let (low, bytes) = unsafe { ((*attr).stack_base, (*attr).stack_pages * 4096) };
        if low == 0 {
            // These attributes were never filled in by `pthread_getattr_np`, so
            // they describe a thread that does not exist and have no stack to name.
            // SAFETY: forwarded from the caller.
            unsafe {
                if !base.is_null() {
                    *base = ptr::null_mut();
                }
                if !size.is_null() {
                    *size = 0;
                }
            }
            return EINVAL;
        }
        // SAFETY: forwarded from the caller.
        unsafe {
            if !base.is_null() {
                *base = low as *mut c_void;
            }
            if !size.is_null() {
                *size = bytes as usize;
            }
        }
        0
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
