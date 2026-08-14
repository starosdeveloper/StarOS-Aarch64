//! Layer 6 of the contract: descriptors you can *wait* on — `poll`, `eventfd`,
//! `pipe` — and the table that holds every descriptor in the process.
//!
//! This is the layer `QEventDispatcherUNIX` stands on. Qt's event loop is a `poll`
//! over a set of descriptors with a timeout, so without a `poll` that really blocks
//! and really wakes, Qt does not turn.
//!
//! ## What a descriptor is here
//! One table, one kind tag. A file is a handle at the file server; an eventfd is a
//! counter; a pipe end is a ring buffer shared by its two ends. Splitting them into
//! separate tables by number range was the other option, and it fails the first
//! time a program calls `dup2(fd, 1)`.
//!
//! ## Why a waitable object cannot allocate its notification
//! A capability table is *copied* into a thread when it is created, so a
//! notification made after that thread started does not exist in it — its
//! `NotifySignal` would name nothing. An `eventfd` created at run time by one
//! thread and waited on by another has to work, so the notifications come from a
//! pool built during start-up, exactly like the thread parkers. That is why
//! `MAX_WAITABLES` is a fixed number rather than "as many as you like": it is the
//! ABI's shape showing through, and the honest failure is `EMFILE` at the limit.
//!
//! ## Readiness is recomputed, never remembered
//! `poll` asks each descriptor whether it is ready *now*, both before it waits and
//! after it wakes. A cached "ready" bit set by a writer is the classic way to build
//! an event loop that reports a stale readiness and then blocks forever in `read`.

use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::lock::Spin;
use crate::sys;

/// Descriptors in the process. Fixed, so no path here allocates.
pub(crate) const MAX_FDS: usize = 32;

/// The first descriptor handed out. 0, 1 and 2 are the standard streams.
pub(crate) const FIRST_FD: c_int = 3;

/// How many waitable objects (eventfds and pipes) may exist at once. Each costs two
/// notifications from the start-up pool — one for each direction.
const MAX_WAITABLES: usize = 8;

/// Bytes a pipe holds before a writer has to wait.
const PIPE_CAPACITY: usize = 4096;

/// `poll` events, as POSIX numbers them.
pub(crate) const POLLIN: i16 = 0x001;
pub(crate) const POLLOUT: i16 = 0x004;
pub(crate) const POLLERR: i16 = 0x008;
pub(crate) const POLLNVAL: i16 = 0x020;

/// `EFD_SEMAPHORE`: read takes one, not the whole counter.
const EFD_SEMAPHORE: c_int = 1;

/// What a descriptor is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Free,
    /// A file at the server: its handle, its size and this descriptor's cursor.
    File { handle: u64, size: u64, offset: u64 },
    /// An eventfd: an index into the waitable pool, and whether it is semaphore-like.
    Event { slot: usize, semaphore: bool },
    /// One end of a pipe.
    Pipe { slot: usize, write: bool },
    /// An IPC endpoint: the capability handle, and the waitable slot whose
    /// `readable` notification the kernel signals when a message arrives.
    ///
    /// The slot's counter and ring go unused here — an endpoint keeps its queue in
    /// the kernel, and the only thing borrowed from the pool is a notification that
    /// exists in every thread. Spending a whole slot on that is waste measured in
    /// one array element, against the alternative of a second pool with its own
    /// exhaustion rules.
    ///
    /// `slot` is `None` for a send-only endpoint. Such a descriptor is a real one —
    /// it can be written, and `poll` calls it writable — it simply has nothing that
    /// could ever make it readable, because this task may not receive there. Two
    /// descriptor kinds for the two directions was the other option, and it makes
    /// every caller ask which one it holds before it can write.
    Endpoint { cap: u32, slot: Option<usize> },
}

/// One waitable object: a counter *and* a ring, because an eventfd needs the first
/// and a pipe the second, and one pool is simpler to reason about than two.
struct Waitable {
    used: AtomicU32,
    /// Signalled when a reader might make progress.
    readable: AtomicU32,
    /// Signalled when a writer might.
    writable: AtomicU32,
    /// The eventfd counter.
    count: AtomicU64,
    /// The pipe ring, and how many ends are still open.
    ring: Spin,
    head: AtomicU64,
    tail: AtomicU64,
    ends: AtomicU32,
}

impl Waitable {
    const fn new() -> Self {
        Self {
            used: AtomicU32::new(0),
            readable: AtomicU32::new(0),
            writable: AtomicU32::new(0),
            count: AtomicU64::new(0),
            ring: Spin::new(),
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            ends: AtomicU32::new(0),
        }
    }

    /// Bytes waiting in the ring.
    fn buffered(&self) -> usize {
        (self.tail.load(Ordering::Acquire) - self.head.load(Ordering::Acquire)) as usize
    }
}

static WAITABLES: [Waitable; MAX_WAITABLES] = [const { Waitable::new() }; MAX_WAITABLES];
/// Ring storage, kept out of `Waitable` so the struct stays `const`-constructible
/// without a 32 KiB static initialiser inside it.
static mut RINGS: [[u8; PIPE_CAPACITY]; MAX_WAITABLES] = [[0; PIPE_CAPACITY]; MAX_WAITABLES];

static mut FDS: [Kind; MAX_FDS] = [Kind::Free; MAX_FDS];
static FDS_LOCK: Spin = Spin::new();

/// Build the notification pool. Called from start-up, before any thread exists —
/// see the module docs.
pub(crate) fn init() {
    for w in &WAITABLES {
        let (Some(readable), Some(writable)) = (sys::notify_create(), sys::notify_create()) else {
            // Out of notifications: the objects that got theirs still work, and
            // `eventfd`/`pipe` past this point fail rather than returning a
            // descriptor whose wake-ups go nowhere.
            break;
        };
        w.readable.store(readable, Ordering::Release);
        w.writable.store(writable, Ordering::Release);
    }
}

/// Install a descriptor, returning its number or `-1` if the table is full.
fn install(kind: Kind) -> c_int {
    let _guard = FDS_LOCK.lock();
    // SAFETY: exclusive under the descriptor lock.
    let fds = unsafe { &mut *core::ptr::addr_of_mut!(FDS) };
    match fds.iter().position(|k| *k == Kind::Free) {
        Some(index) => {
            fds[index] = kind;
            index as c_int + FIRST_FD
        }
        None => -1,
    }
}

/// What a descriptor names, or `None`.
pub(crate) fn get(fd: c_int) -> Option<Kind> {
    let index = usize::try_from(fd - FIRST_FD).ok()?;
    let _guard = FDS_LOCK.lock();
    // SAFETY: read under the descriptor lock.
    let fds = unsafe { &*core::ptr::addr_of!(FDS) };
    match fds.get(index)? {
        Kind::Free => None,
        kind => Some(*kind),
    }
}

/// Replace what a descriptor names.
fn set(fd: c_int, kind: Kind) -> bool {
    let Ok(index) = usize::try_from(fd - FIRST_FD) else {
        return false;
    };
    let _guard = FDS_LOCK.lock();
    // SAFETY: exclusive under the descriptor lock.
    let fds = unsafe { &mut *core::ptr::addr_of_mut!(FDS) };
    match fds.get_mut(index) {
        Some(slot) => {
            *slot = kind;
            true
        }
        None => false,
    }
}

/// Install a file descriptor for a handle the file layer opened.
pub(crate) fn install_file(handle: u64, size: u64) -> c_int {
    install(Kind::File { handle, size, offset: 0 })
}

/// Update a file descriptor's cursor.
pub(crate) fn set_offset(fd: c_int, offset: u64) -> bool {
    match get(fd) {
        Some(Kind::File { handle, size, .. }) => set(fd, Kind::File { handle, size, offset }),
        _ => false,
    }
}

/// Take a free waitable slot.
fn take_waitable() -> Option<usize> {
    for (i, w) in WAITABLES.iter().enumerate() {
        if w.readable.load(Ordering::Acquire) == 0 {
            continue; // never got its notifications
        }
        if w.used
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            w.count.store(0, Ordering::Release);
            w.head.store(0, Ordering::Release);
            w.tail.store(0, Ordering::Release);
            w.ends.store(0, Ordering::Release);
            return Some(i);
        }
    }
    None
}

/// Is this descriptor ready for the events asked about?
fn readiness(kind: Kind, wanted: i16) -> i16 {
    let mut ready = 0;
    match kind {
        Kind::Free => return POLLNVAL,
        // A file at a read-only server is always readable and never writable: the
        // read may return zero bytes at end of file, which is what "readable" means
        // for a file everywhere else too.
        Kind::File { .. } => ready |= POLLIN,
        Kind::Event { slot, .. } => {
            let w = &WAITABLES[slot];
            if w.count.load(Ordering::Acquire) > 0 {
                ready |= POLLIN;
            }
            // An eventfd is writable until its counter is one short of saturating;
            // ours never gets there in practice, so it is always writable.
            ready |= POLLOUT;
        }
        Kind::Pipe { slot, write } => {
            let w = &WAITABLES[slot];
            if write {
                if w.buffered() < PIPE_CAPACITY {
                    ready |= POLLOUT;
                }
                // The reading end closed: writers must hear about it rather than
                // filling a ring nobody will drain.
                if w.ends.load(Ordering::Acquire) < 2 {
                    ready |= POLLERR;
                }
            } else {
                if w.buffered() > 0 {
                    ready |= POLLIN;
                }
                // The writing end closed and the ring is drained: end of file, which
                // `poll` reports as readable so the reader gets its zero-length read.
                if w.ends.load(Ordering::Acquire) < 2 && w.buffered() == 0 {
                    ready |= POLLIN;
                }
            }
        }
        // Asked of the kernel every time, never remembered. The bound notification
        // says "something happened"; only the queue says "something is still here",
        // and the difference is a message another thread took between the wake-up
        // and this question.
        Kind::Endpoint { cap, .. } => {
            if sys::endpoint_pending(cap) > 0 {
                ready |= POLLIN;
            }
            // An endpoint is always writable in the sense `poll` means: a send may
            // still block when the ring is full, exactly as a write to a socket
            // may, and there is no way to ask the kernel about a queue we are not
            // the receiver of.
            ready |= POLLOUT;
        }
    }
    ready & (wanted | POLLERR | POLLNVAL)
}

/// The notification a descriptor's waiter should block on, if any.
fn notification(kind: Kind, wanted: i16) -> Option<u32> {
    let (slot, want_read) = match kind {
        Kind::Event { slot, .. } => (slot, wanted & POLLIN != 0),
        Kind::Pipe { slot, write } => (slot, !write),
        // Only the arrival of a message can be waited for. A caller that asks to be
        // told when an endpoint becomes *writable* is asking about a queue in
        // another task's future, and gets the timeout it deserves rather than a
        // wake-up this side cannot promise.
        Kind::Endpoint { slot: Some(slot), .. } => (slot, true),
        Kind::Endpoint { slot: None, .. } => return None,
        _ => return None,
    };
    let w = &WAITABLES[slot];
    Some(if want_read {
        w.readable.load(Ordering::Acquire)
    } else {
        w.writable.load(Ordering::Acquire)
    })
    .filter(|h| *h != 0)
}

/// `struct pollfd`.
#[repr(C)]
pub struct PollFd {
    pub fd: c_int,
    pub events: i16,
    pub revents: i16,
}

/// The shared body of `poll` and `ppoll`.
///
/// `deadline` is absolute nanoseconds, `None` for "wait forever", `Some(0)` for
/// "do not wait".
pub(crate) fn poll_impl(fds: &mut [PollFd], deadline: Option<u64>) -> c_int {
    /// The kernel takes at most this many sources in one `WaitAny`.
    const MAX_SOURCES: usize = 16;

    loop {
        // Recompute readiness every time round, before and after any wait: a bit
        // remembered from last time is how an event loop comes to report a ready
        // descriptor and then block in `read`.
        let mut ready = 0;
        for entry in fds.iter_mut() {
            entry.revents = match get(entry.fd) {
                Some(kind) => readiness(kind, entry.events),
                None if entry.fd < 0 => 0,
                None => POLLNVAL,
            };
            if entry.revents != 0 {
                ready += 1;
            }
        }
        if ready > 0 {
            return ready;
        }
        if deadline == Some(0) {
            return 0;
        }

        // Nothing ready: collect the sources to sleep on.
        let mut handles = [0u32; MAX_SOURCES];
        let mut count = 0;
        for entry in fds.iter() {
            let Some(kind) = get(entry.fd) else { continue };
            let Some(handle) = notification(kind, entry.events) else {
                continue;
            };
            if !handles[..count].contains(&handle) && count < MAX_SOURCES {
                handles[count] = handle;
                count += 1;
            }
        }
        if count == 0 {
            // Only un-waitable descriptors (files, or none at all). Sleeping until
            // the deadline is the honest answer — there is nothing that could wake
            // this — and a caller polling a set of files gets its timeout rather
            // than a busy loop.
            match deadline {
                Some(d) => {
                    sys::sleep_until(d);
                    return 0;
                }
                None => {
                    // Waiting forever on nothing is a deadlock the caller asked for;
                    // refusing is more useful than providing it.
                    return -1;
                }
            }
        }

        let fired = sys::wait_any(&handles[..count], deadline.unwrap_or(0));
        if !fired && deadline.is_some() {
            // The deadline passed with nothing signalled. One last readiness pass
            // happens at the top of the loop only if we continue, so report the
            // timeout here.
            for entry in fds.iter_mut() {
                entry.revents = 0;
            }
            return 0;
        }
    }
}

/// Read from a waitable descriptor. Returns bytes read, or `-1`.
pub(crate) fn read(kind: Kind, dst: &mut [u8]) -> isize {
    match kind {
        Kind::Event { slot, semaphore } => {
            if dst.len() < 8 {
                return -1;
            }
            let w = &WAITABLES[slot];
            loop {
                let count = w.count.load(Ordering::Acquire);
                if count > 0 {
                    let take = if semaphore { 1 } else { count };
                    if w.count
                        .compare_exchange(count, count - take, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                    {
                        continue;
                    }
                    dst[..8].copy_from_slice(&take.to_ne_bytes());
                    sys::notify_signal(w.writable.load(Ordering::Acquire));
                    return 8;
                }
                sys::wait(w.readable.load(Ordering::Acquire));
            }
        }
        Kind::Pipe { slot, write: false } => {
            let w = &WAITABLES[slot];
            loop {
                {
                    let _guard = w.ring.lock();
                    let buffered = w.buffered();
                    if buffered > 0 {
                        let n = buffered.min(dst.len());
                        let head = w.head.load(Ordering::Acquire) as usize;
                        for (i, byte) in dst[..n].iter_mut().enumerate() {
                            // SAFETY: the ring is this slot's, indexed modulo its
                            // size, under the ring lock.
                            *byte = unsafe {
                                (*core::ptr::addr_of!(RINGS))[slot][(head + i) % PIPE_CAPACITY]
                            };
                        }
                        w.head.store((head + n) as u64, Ordering::Release);
                        sys::notify_signal(w.writable.load(Ordering::Acquire));
                        return n as isize;
                    }
                    if w.ends.load(Ordering::Acquire) < 2 {
                        return 0; // the writer is gone: end of file
                    }
                }
                sys::wait(w.readable.load(Ordering::Acquire));
            }
        }
        // One message, whole or not at all. A short read of a message is not a
        // shorter message, it is a corrupt one, and the caller has no way to ask for
        // the rest: the kernel handed it over already.
        Kind::Endpoint { cap, .. } => {
            if dst.len() < size_of::<sys::Message>() {
                return -1;
            }
            match sys::recv(u64::from(cap)) {
                Some(msg) => {
                    // SAFETY: `dst` has room for a whole `Message`, checked above,
                    // and `Message` is `repr(C)` with no padding the caller may not
                    // see — it is the same bytes the kernel wrote.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            core::ptr::from_ref(&msg).cast::<u8>(),
                            dst.as_mut_ptr(),
                            size_of::<sys::Message>(),
                        );
                    }
                    size_of::<sys::Message>() as isize
                }
                None => -1,
            }
        }
        _ => -1,
    }
}

/// Write to a waitable descriptor. Returns bytes written, or `-1`.
pub(crate) fn write(kind: Kind, src: &[u8]) -> isize {
    match kind {
        Kind::Event { slot, .. } => {
            if src.len() < 8 {
                return -1;
            }
            let mut value = [0u8; 8];
            value.copy_from_slice(&src[..8]);
            let w = &WAITABLES[slot];
            w.count.fetch_add(u64::from_ne_bytes(value), Ordering::AcqRel);
            sys::notify_signal(w.readable.load(Ordering::Acquire));
            8
        }
        Kind::Pipe { slot, write: true } => {
            let w = &WAITABLES[slot];
            let mut written = 0;
            while written < src.len() {
                {
                    let _guard = w.ring.lock();
                    if w.ends.load(Ordering::Acquire) < 2 {
                        // Nobody is reading. POSIX raises SIGPIPE here; there are no
                        // signals, so the write fails, which is the other half of
                        // what a caller checks.
                        return if written > 0 { written as isize } else { -1 };
                    }
                    let space = PIPE_CAPACITY - w.buffered();
                    if space > 0 {
                        let n = space.min(src.len() - written);
                        let tail = w.tail.load(Ordering::Acquire) as usize;
                        for i in 0..n {
                            // SAFETY: as in `read`.
                            unsafe {
                                (*core::ptr::addr_of_mut!(RINGS))[slot]
                                    [(tail + i) % PIPE_CAPACITY] = src[written + i];
                            }
                        }
                        w.tail.store((tail + n) as u64, Ordering::Release);
                        written += n;
                        sys::notify_signal(w.readable.load(Ordering::Acquire));
                        continue;
                    }
                }
                sys::wait(w.writable.load(Ordering::Acquire));
            }
            written as isize
        }
        Kind::Endpoint { cap, .. } => {
            if src.len() < size_of::<sys::Message>() {
                return -1;
            }
            let mut msg = sys::Message::new();
            // SAFETY: `src` holds at least one `Message`, checked above; the copy is
            // into a local of exactly that type and alignment.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src.as_ptr(),
                    core::ptr::from_mut(&mut msg).cast::<u8>(),
                    size_of::<sys::Message>(),
                );
            }
            if sys::send(u64::from(cap), &msg) < 0 {
                -1
            } else {
                size_of::<sys::Message>() as isize
            }
        }
        _ => -1,
    }
}

/// Release a descriptor. Returns the file handle to close, if it was a file.
pub(crate) fn release(fd: c_int) -> Option<u64> {
    let kind = get(fd)?;
    set(fd, Kind::Free);
    match kind {
        Kind::File { handle, .. } => Some(handle),
        Kind::Event { slot, .. } => {
            WAITABLES[slot].used.store(0, Ordering::Release);
            None
        }
        Kind::Pipe { slot, .. } => {
            let w = &WAITABLES[slot];
            // One end fewer. The other end's readiness changes because of it, so
            // wake both directions: a reader blocked on a pipe whose writer just
            // closed must get its end of file rather than sleeping forever.
            if w.ends.fetch_sub(1, Ordering::AcqRel) <= 1 {
                w.used.store(0, Ordering::Release);
            }
            sys::notify_signal(w.readable.load(Ordering::Acquire));
            sys::notify_signal(w.writable.load(Ordering::Acquire));
            None
        }
        // The slot comes back; the binding in the kernel does not, and cannot until
        // an endpoint can be unbound. A signal to a notification whose slot has been
        // reused is a spurious wake-up for its next owner, which costs one trip
        // round a poll loop — the price of not adding an unbind syscall for a
        // descriptor that in practice lives as long as the program.
        Kind::Endpoint { slot, .. } => {
            if let Some(slot) = slot {
                WAITABLES[slot].used.store(0, Ordering::Release);
            }
            None
        }
        Kind::Free => None,
    }
}

/// The C entry points.
#[cfg(not(test))]
pub mod exports {
    use super::{
        c_int, c_void, get, install, poll_impl, take_waitable, Kind, PollFd, EFD_SEMAPHORE,
        WAITABLES,
    };
    use core::sync::atomic::Ordering;

    /// `EMFILE`: no descriptor or no waitable slot left.
    const EMFILE: c_int = 24;
    /// `ENOSYS`, for what this system does not have.
    const ENOSYS: c_int = 38;
    /// `EINVAL`.
    const EINVAL: c_int = 22;

    /// # Safety
    /// C ABI: `fds` is an array of `nfds` `pollfd`s.
    #[no_mangle]
    pub unsafe extern "C" fn poll(fds: *mut PollFd, nfds: usize, timeout_ms: c_int) -> c_int {
        // SAFETY: forwarded from the caller.
        let entries = unsafe { core::slice::from_raw_parts_mut(fds, nfds) };
        let deadline = match timeout_ms {
            // Negative means "no timeout"; zero means "do not block". Both are
            // POSIX, and confusing them gives an event loop that either spins or
            // hangs.
            t if t < 0 => None,
            0 => Some(0),
            t => crate::sys::clock_now().map(|now| now + (t as u64) * 1_000_000),
        };
        poll_impl(entries, deadline)
    }

    /// # Safety
    /// C ABI: as [`poll`]; `timeout` is a *relative* `timespec` and the signal mask
    /// is ignored, there being no signals.
    #[no_mangle]
    pub unsafe extern "C" fn ppoll(
        fds: *mut PollFd,
        nfds: usize,
        timeout: *const crate::time::Timespec,
        _sigmask: *const c_void,
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        let entries = unsafe { core::slice::from_raw_parts_mut(fds, nfds) };
        let deadline = if timeout.is_null() {
            None
        } else {
            // SAFETY: forwarded from the caller.
            let want = unsafe { crate::time::join((*timeout).tv_sec, (*timeout).tv_nsec) };
            if want == 0 {
                Some(0)
            } else {
                crate::sys::clock_now().map(|now| now.saturating_add(want))
            }
        };
        poll_impl(entries, deadline)
    }

    #[no_mangle]
    pub extern "C" fn eventfd(initial: c_int, flags: c_int) -> c_int {
        let Some(slot) = take_waitable() else {
            return -EMFILE;
        };
        WAITABLES[slot]
            .count
            .store(initial.max(0) as u64, Ordering::Release);
        let fd = install(Kind::Event { slot, semaphore: flags & EFD_SEMAPHORE != 0 });
        if fd < 0 {
            WAITABLES[slot].used.store(0, Ordering::Release);
            return -EMFILE;
        }
        fd
    }

    /// Wrap an IPC endpoint capability in a descriptor `poll` can wait on.
    ///
    /// This is the one call that lets a POSIX event loop see this system's native
    /// communication primitive. `QEventDispatcherUNIX` waits in `poll`; messages
    /// arrive at endpoints; without a descriptor in between, a program can serve
    /// messages or run an event loop, but not both.
    ///
    /// `read` on the returned descriptor delivers one whole message (48 bytes, laid
    /// out as the kernel's `Message`), `write` sends one, and `poll` reports
    /// `POLLIN` while the endpoint's queue is not empty. Returns `EMFILE` negated if
    /// the descriptor table or the waitable pool is full, and `EINVAL` negated if
    /// the kernel refused the binding — a handle that is not ours, or one with no
    /// receive rights.
    #[no_mangle]
    pub extern "C" fn staros_endpoint_fd(cap: u32) -> c_int {
        /// What the kernel returns when the endpoint carries no receive rights.
        const PERMISSION_DENIED: isize = -3;

        let Some(slot) = take_waitable() else {
            return -EMFILE;
        };
        let notification = WAITABLES[slot].readable.load(Ordering::Acquire);
        if notification == 0 {
            WAITABLES[slot].used.store(0, Ordering::Release);
            return -EMFILE;
        }
        let slot = match crate::sys::endpoint_bind(cap, notification) {
            0 => Some(slot),
            // Send-only: give the slot straight back and make a write-only
            // descriptor. The capability is good, it just points the other way.
            PERMISSION_DENIED => {
                WAITABLES[slot].used.store(0, Ordering::Release);
                None
            }
            // Anything else means the handle is not ours, or names no endpoint.
            _ => {
                WAITABLES[slot].used.store(0, Ordering::Release);
                return -EINVAL;
            }
        };
        let fd = install(Kind::Endpoint { cap, slot });
        if fd < 0 {
            if let Some(slot) = slot {
                WAITABLES[slot].used.store(0, Ordering::Release);
            }
            return -EMFILE;
        }
        fd
    }

    /// # Safety
    /// C ABI: `value` receives the counter.
    #[no_mangle]
    pub unsafe extern "C" fn eventfd_read(fd: c_int, value: *mut u64) -> c_int {
        let Some(kind @ Kind::Event { .. }) = get(fd) else {
            return -EINVAL;
        };
        let mut buf = [0u8; 8];
        if super::read(kind, &mut buf) != 8 {
            return -EINVAL;
        }
        // SAFETY: forwarded from the caller.
        unsafe { *value = u64::from_ne_bytes(buf) };
        0
    }

    #[no_mangle]
    pub extern "C" fn eventfd_write(fd: c_int, value: u64) -> c_int {
        let Some(kind @ Kind::Event { .. }) = get(fd) else {
            return -EINVAL;
        };
        if super::write(kind, &value.to_ne_bytes()) != 8 {
            return -EINVAL;
        }
        0
    }

    /// # Safety
    /// C ABI: `fds` receives the read end then the write end.
    #[no_mangle]
    pub unsafe extern "C" fn pipe2(fds: *mut c_int, _flags: c_int) -> c_int {
        let Some(slot) = take_waitable() else {
            return -EMFILE;
        };
        WAITABLES[slot].ends.store(2, Ordering::Release);
        let read_end = install(Kind::Pipe { slot, write: false });
        let write_end = install(Kind::Pipe { slot, write: true });
        if read_end < 0 || write_end < 0 {
            if read_end >= 0 {
                super::release(read_end);
            }
            if write_end >= 0 {
                super::release(write_end);
            }
            WAITABLES[slot].used.store(0, Ordering::Release);
            return -EMFILE;
        }
        // SAFETY: forwarded from the caller.
        unsafe {
            *fds = read_end;
            *fds.add(1) = write_end;
        }
        0
    }

    /// # Safety
    /// As [`pipe2`].
    #[no_mangle]
    pub unsafe extern "C" fn pipe(fds: *mut c_int) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { pipe2(fds, 0) }
    }

    /// Duplicate a descriptor.
    ///
    /// The copy shares the underlying object — that is what `dup` means — but this
    /// table has no reference count, so closing either copy releases it. Recorded
    /// here rather than discovered: a program that dups a pipe and closes one copy
    /// gets an end-of-file it did not ask for.
    #[no_mangle]
    pub extern "C" fn dup(fd: c_int) -> c_int {
        match get(fd) {
            Some(kind) => install(kind),
            None => -EINVAL,
        }
    }

    #[no_mangle]
    pub extern "C" fn dup3(old: c_int, new: c_int, _flags: c_int) -> c_int {
        let Some(kind) = get(old) else {
            return -EINVAL;
        };
        if old == new {
            return new;
        }
        // Replacing an arbitrary descriptor number would need the table to be
        // indexed by that number, which it is; but `new` may name a live descriptor
        // whose object then leaks. Closing it first is what POSIX says.
        super::release(new);
        if super::set(new, kind) {
            new
        } else {
            -EINVAL
        }
    }

    #[no_mangle]
    pub extern "C" fn dup2(old: c_int, new: c_int) -> c_int {
        dup3(old, new, 0)
    }

    #[no_mangle]
    pub extern "C" fn close_range(first: c_int, last: c_int, _flags: c_int) -> c_int {
        for fd in first..=last {
            super::release(fd);
        }
        0
    }

    // Watching a filesystem that cannot change is a service with nothing to report.
    // These exist so a program links; they refuse so a program that depends on them
    // finds out immediately rather than waiting for events that will never come.
    #[no_mangle]
    pub extern "C" fn inotify_init() -> c_int {
        -ENOSYS
    }

    #[no_mangle]
    pub extern "C" fn inotify_init1(_flags: c_int) -> c_int {
        -ENOSYS
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn inotify_add_watch(
        _fd: c_int,
        _path: *const c_void,
        _mask: u32,
    ) -> c_int {
        -ENOSYS
    }

    #[no_mangle]
    pub extern "C" fn inotify_rm_watch(_fd: c_int, _wd: c_int) -> c_int {
        -ENOSYS
    }
}
