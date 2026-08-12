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
