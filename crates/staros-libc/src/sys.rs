//! The syscall layer: the only place in this crate that knows how to talk to the
//! kernel.
//!
//! Numbers come from `staros_abi::syscall::Syscall` rather than being written out
//! again here. A libc with its own copy of the numbers is a libc that keeps working
//! after the ABI changes, right up until it does something else entirely.

use staros_abi::syscall::Syscall;

/// A message, laid out exactly as `staros_ipc::Message`: tag, four words, then a
/// capability handle.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Message {
    pub tag: u64,
    pub words: [u64; 4],
    pub cap: u32,
}

impl Message {
    pub(crate) const fn new() -> Self {
        Self { tag: 0, words: [0; 4], cap: 0 }
    }
}

/// Issue a syscall with up to three arguments.
///
/// # Safety
/// The number and arguments must name a syscall this process may make. The kernel
/// validates every pointer against the caller's own page tables, so a bad pointer
/// is an error return rather than a corrupted kernel — but a bad *handle* is still
/// this program's problem.
#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn syscall3(number: Syscall, a0: u64, a1: u64, a2: u64) -> isize {
    let ret;
    // SAFETY: the kernel's SVC handler preserves every register but x0.
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") number as usize,
            inout("x0") a0 => ret,
            in("x1") a1,
            in("x2") a2,
            options(nostack),
        );
    }
    ret
}

/// The host build compiles this crate for its tests; nothing there may call a
/// syscall, and saying so loudly beats a silent zero.
#[cfg(not(target_arch = "aarch64"))]
pub(crate) unsafe fn syscall3(_number: Syscall, _a0: u64, _a1: u64, _a2: u64) -> isize {
    unimplemented!("syscalls are only available on the target")
}

/// A four-argument syscall. Only `SpawnThread` needs the fourth register.
///
/// # Safety
/// As [`syscall3`].
#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn syscall4(number: Syscall, a0: u64, a1: u64, a2: u64, a3: u64) -> isize {
    let ret;
    // SAFETY: the kernel's SVC handler preserves every register but x0.
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") number as usize,
            inout("x0") a0 => ret,
            in("x1") a1,
            in("x2") a2,
            in("x3") a3,
            options(nostack),
        );
    }
    ret
}

#[cfg(not(target_arch = "aarch64"))]
pub(crate) unsafe fn syscall4(_n: Syscall, _a0: u64, _a1: u64, _a2: u64, _a3: u64) -> isize {
    unimplemented!("syscalls are only available on the target")
}

pub(crate) unsafe fn syscall0(number: Syscall) -> isize {
    // SAFETY: forwarded from the caller.
    unsafe { syscall3(number, 0, 0, 0) }
}

pub(crate) unsafe fn syscall1(number: Syscall, a0: u64) -> isize {
    // SAFETY: forwarded from the caller.
    unsafe { syscall3(number, a0, 0, 0) }
}

pub(crate) unsafe fn syscall2(number: Syscall, a0: u64, a1: u64) -> isize {
    // SAFETY: forwarded from the caller.
    unsafe { syscall3(number, a0, a1, 0) }
}

/// Write a run of bytes to the debug console in one syscall. One call is one line
/// as far as the console lock is concerned, which is why `stdio` buffers.
pub(crate) fn debug_write(bytes: &[u8]) {
    // SAFETY: `DebugWrite` reads `len` bytes from `ptr` after walking our tables.
    unsafe {
        syscall2(Syscall::DebugWrite, bytes.as_ptr() as u64, bytes.len() as u64);
    }
}

/// Map `pages` of fresh anonymous memory and return its address, or `None`.
pub(crate) fn map_anon(pages: usize) -> Option<*mut u8> {
    // SAFETY: `MapAnon` allocates and maps; a refusal is a negative return.
    let rc = unsafe { syscall1(Syscall::MapAnon, pages as u64) };
    (rc > 0).then(|| rc as usize as *mut u8)
}

/// Nanoseconds since boot on the monotonic clock, or `None` if the kernel has no
/// clock yet.
pub(crate) fn clock_now() -> Option<u64> {
    // SAFETY: `ClockNow` takes no arguments and needs no capability.
    let rc = unsafe { syscall0(Syscall::ClockNow) };
    (rc > 0).then_some(rc as u64)
}

/// Sleep until an absolute deadline on that same clock.
pub(crate) fn sleep_until(deadline_ns: u64) {
    // SAFETY: `SleepUntil` parks this task; a deadline in the past returns at once.
    unsafe {
        syscall1(Syscall::SleepUntil, deadline_ns);
    }
}

/// End this process. Never returns.
pub(crate) fn exit(code: i32) -> ! {
    // SAFETY: `Exit` does not return.
    unsafe {
        syscall1(Syscall::Exit, code as u64);
    }
    loop {
        core::hint::spin_loop();
    }
}

/// Create a shared buffer of `pages` pages and return its capability handle.
pub(crate) fn create_shared(pages: usize) -> Option<u32> {
    // SAFETY: `CreateShared` allocates zeroed frames and returns a handle.
    let rc = unsafe { syscall1(Syscall::CreateShared, pages as u64) };
    (rc > 0).then_some(rc as u32)
}

/// Map a shared buffer this process holds a capability for.
pub(crate) fn map_shared(cap: u32) -> Option<*mut u8> {
    // SAFETY: `MapShared` maps the object the handle names.
    let rc = unsafe { syscall1(Syscall::MapShared, u64::from(cap)) };
    (rc > 0).then(|| rc as usize as *mut u8)
}

/// Create a notification of our own and return its capability handle.
///
/// Capability tables are *copied* into a new thread at `SpawnThread`, so a handle
/// created before any thread exists names the same object in every thread, and one
/// created afterwards exists only in its creator. That is the whole reason the
/// thread layer allocates its notifications up front.
pub(crate) fn notify_create() -> Option<u32> {
    // SAFETY: `NotifyCreate` takes no arguments and needs no capability.
    let rc = unsafe { syscall0(Syscall::NotifyCreate) };
    (rc > 0).then_some(rc as u32)
}

/// Signal a notification. A signal that arrives before anyone waits is counted,
/// not lost — which is what makes the park/unpark pairs below race-free.
pub(crate) fn notify_signal(handle: u32) {
    // SAFETY: `NotifySignal` only looks the handle up in our own table.
    unsafe {
        syscall1(Syscall::NotifySignal, u64::from(handle));
    }
}

/// Block until a notification is signalled, consuming one signal.
pub(crate) fn wait(handle: u32) {
    // SAFETY: `Wait` parks this thread until the notification fires.
    unsafe {
        syscall1(Syscall::Wait, u64::from(handle));
    }
}

/// Block until a notification is signalled or the absolute deadline passes.
/// Returns `true` if the notification fired.
pub(crate) fn wait_until(handle: u32, deadline_ns: u64) -> bool {
    wait_any(&[handle], deadline_ns)
}

/// Block until any of `handles` is signalled, or the absolute deadline passes.
/// Returns `true` if one fired. A deadline of zero means "no deadline" — the
/// kernel's convention, because a deadline already in the past would otherwise turn
/// every wait into a poll that never waits.
pub(crate) fn wait_any(handles: &[u32], deadline_ns: u64) -> bool {
    // SAFETY: `WaitAny` reads `len` handles from the pointer and parks with a
    // deadline; the slice outlives the call.
    let rc = unsafe {
        syscall3(Syscall::WaitAny, handles.as_ptr() as u64, handles.len() as u64, deadline_ns)
    };
    rc >= 0
}

/// Start a thread in this address space: `entry` receives `arg` in `x0`, on a
/// fresh stack of `stack_pages` pages, with `TPIDR_EL0` set to `tls`.
pub(crate) fn spawn_thread(entry: u64, stack_pages: u64, tls: u64, arg: u64) -> isize {
    // SAFETY: `SpawnThread` maps the stack itself; `entry` must be code in this
    // space and `tls` a thread block this process owns, which the caller
    // guarantees.
    unsafe { syscall4(Syscall::SpawnThread, entry, stack_pages, tls, arg) }
}

/// This process's id, the same number in every one of its threads.
pub(crate) fn task_id() -> i64 {
    // SAFETY: `TaskId` takes no arguments and always returns.
    unsafe { syscall0(Syscall::TaskId) as i64 }
}

/// Give up the rest of this timeslice.
pub(crate) fn yield_now() {
    // SAFETY: `Yield` reschedules and returns.
    unsafe {
        syscall0(Syscall::Yield);
    }
}

/// Send a message on an endpoint handle.
pub(crate) fn send(endpoint: u64, msg: &Message) -> isize {
    // SAFETY: `Send` reads one `Message` through this pointer.
    unsafe { syscall2(Syscall::Send, endpoint, core::ptr::from_ref(msg) as u64) }
}

/// Receive a message on an endpoint handle, blocking until one arrives.
pub(crate) fn recv(endpoint: u64) -> Option<Message> {
    let mut msg = Message::new();
    // SAFETY: `Recv` writes one `Message` through this pointer.
    let rc = unsafe { syscall2(Syscall::Recv, endpoint, core::ptr::from_mut(&mut msg) as u64) };
    (rc >= 0).then_some(msg)
}
