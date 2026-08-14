//! `staros-libc` — the C library EL0 programs link against.
//!
//! This is phase G5 of `docs/ROADMAP-QML.md`, the phase whose risk is that it never
//! ends. The antidote written into the roadmap is to measure instead of guess, so
//! the work has a contract: `docs/libc-contract.txt` lists every C library symbol a
//! real Qt 6 build leaves undefined, and `scripts/libc-progress.sh` scores this
//! library against it. What is implemented is implemented; what is not is missing
//! from the score rather than from memory.
//!
//! ## What is here
//! All seven layers, 282 of 282:
//!
//! * **1, pure computation** — `mem*`, `str*`, `strtol`, `strtod`, the `printf` and
//!   `scanf` engines, and the whole of libm ([`math`]), written rather than borrowed.
//! * **2, memory** — `malloc`/`free`/`calloc`/`realloc`/`aligned_alloc` over
//!   `MapAnon`, plus [`mmap`] for the callers that want pages rather than bytes.
//! * **3, time** — `clock_gettime`/`nanosleep`/`time` over `ClockNow`/`SleepUntil`,
//!   and the calendar (`gmtime_r`, `mktime`, `strftime`).
//! * **4, files** — `open`/`read`/`lseek`/`fstat`/`close` as IPC to `services/fssrv`,
//!   the buffered `FILE*` layer over them ([`stream`]), directories over a flat
//!   archive ([`dir`]), and `stdout`/`stderr` through `DebugWrite`.
//! * **5, threads and TLS** — `pthread_*` over `SpawnThread`, with the AArch64
//!   variant-I thread-local layout the linker's offsets assume.
//! * **6, multiplexing** — `poll`/`eventfd`/`pipe` over `WaitAny`, which is what
//!   `QEventDispatcherUNIX` is built on.
//! * **7, process and system** — the environment, `getpid`, `uname`, `getrlimit`,
//!   `setjmp`/`longjmp`, `backtrace`, the signal-set arithmetic ([`proc`]).
//!
//! ## What is refused, and why that is not the same as missing
//! A symbol being present is not a claim that the operation exists here. `fork`
//! returns `ENOSYS`, `dlopen` fails and `dlerror` explains why in a sentence,
//! writing to a file is `EROFS`, `wait` is `ECHILD`. Every one of those is a
//! decision recorded in `docs/LIBC-CONTRACT.md` with its reason, and every one sets
//! the errno that tells a caller which fallback to take. A stub that returned
//! success would let a program link and then fail somewhere unrelated, which is the
//! failure mode this whole tree is built to avoid.
//!
//! ## Testing
//! Everything that can be tested without a kernel is: the string functions, the
//! format engine (through a `Sink` and an `Args` source rather than a `VaList`), the
//! allocator (through a page source the tests back with a `Vec`), and the time
//! arithmetic. The C-ABI wrappers are compiled only for the target, because a host
//! test binary that defined `memcpy` would collide with the system's own.

#![cfg_attr(not(test), no_std)]
// Under `cargo test` only the pure layers are compiled and run; everything that
// speaks to the kernel is `cfg(not(test))` and its helpers therefore look dead. The
// build that ships is the target one, and it keeps every warning on — `kclippy -D
// warnings` covers it.
#![cfg_attr(test, allow(dead_code, unused_imports))]
// Needed by the variadic `printf` family, which the host build does not compile.
#![cfg_attr(not(test), feature(c_variadic))]
// This crate *defines* `memcpy`, `memset` and `memmove`. Without `no_builtins`,
// LLVM recognises the loops that implement them and replaces each with a call to
// the very function being defined — infinite recursion that ends at the stack
// guard, several function calls away from anything that looks related.
#![cfg_attr(not(test), no_builtins)]

pub mod ctype;
pub mod cxx;
pub mod dir;
pub mod fd;
pub mod fmt;
pub mod file;
pub mod heap;
pub(crate) mod lock;
pub mod locale;
pub mod math;
pub mod mmap;
pub mod proc;
pub mod scan;
pub mod sort;
pub mod stdio;
pub mod stream;
pub mod string;
pub mod surface;
pub mod sys;
pub mod thread;
pub mod time;
pub mod wctype;

/// Allocate from the process heap, aligned, for the parts of this library that
/// need memory before C does — thread control blocks and TLS.
///
/// It exists so `thread` does not reach into the allocator's static directly; there
/// is exactly one place that does, and this is it.
#[cfg(not(test))]
pub(crate) fn heap_alloc(size: usize, align: usize) -> *mut u8 {
    let _guard = HEAP_LOCK.lock();
    // SAFETY: the lock is what makes this exclusive; every other user of `HEAP`
    // takes it too.
    unsafe { (*core::ptr::addr_of_mut!(HEAP)).alloc_aligned(size, align) }
}

/// The host build has no `MapAnon`, so the same call goes to the test harness's
/// allocator. It exists so `thread` compiles under `cargo test` — nothing there
/// runs a thread, and the arithmetic that *is* tested lives in `heap`.
#[cfg(test)]
pub(crate) fn heap_alloc(size: usize, align: usize) -> *mut u8 {
    // SAFETY: a non-zero size with a power-of-two alignment is a valid layout.
    unsafe { std::alloc::alloc(std::alloc::Layout::from_size_align(size.max(1), align).unwrap()) }
}

use core::ffi::{c_int, c_void};

/// The process-wide allocator's page source: anonymous memory from the kernel.
struct KernelPages;

impl heap::Pages for KernelPages {
    fn map(&mut self, bytes: usize) -> Option<*mut u8> {
        sys::map_anon(bytes.div_ceil(self.page_size()))
    }

    fn page_size(&self) -> usize {
        4096
    }
}

/// The process heap, and the lock that makes it safe to share.
///
/// The lock arrived with layer 5 and not before, which is the honest order: until
/// `pthread_create` existed there was no second thread to race with, and a lock
/// nothing contends is a claim nothing tests.
static mut HEAP: heap::Heap<KernelPages> = heap::Heap::new(KernelPages);
static HEAP_LOCK: lock::Spin = lock::Spin::new();

/// `errno`.
///
/// It is one word for the whole process rather than one per thread, which is a real
/// difference from a POSIX libc: two threads failing at once can overwrite each
/// other's code. Making it thread-local is a one-line change once something needs
/// it — the thread-local ABI has existed since layer 5 — and until a caller reads
/// `errno` across a thread boundary the change would be untested.
static mut ERRNO: c_int = 0;

/// Set `errno` and return the failure value the caller wants, in one expression.
///
/// Every failing entry point in this library goes through here, so "which calls set
/// errno" has one answer that can be read off the call sites rather than guessed.
#[cfg(not(test))]
pub(crate) fn fail<T>(code: c_int, value: T) -> T {
    // SAFETY: a single word; see the note on `ERRNO` about threads.
    unsafe { ERRNO = code };
    value
}

/// The C runtime entry point.
///
/// The kernel `eret`s here with a fresh stack. This sets up what C expects to exist
/// before `main` — the file-server connection — calls `main`, flushes whatever the
/// program printed without a newline, and exits with its status. A C program in
/// this system therefore starts at `int main(void)` and nothing else.
///
/// # Safety
/// Called by the kernel, once, on a stack it just mapped.
#[cfg(not(test))]
#[no_mangle]
pub unsafe extern "C" fn _start() -> ! {
    extern "C" {
        fn main(argc: c_int, argv: *mut *mut core::ffi::c_char) -> c_int;
    }
    // Threads first: it builds the parker pool, and a notification created after a
    // thread exists is one that thread cannot see. It also gives `main` its thread
    // pointer, so `_Thread_local` works in `main` and not only in what it spawns.
    thread::init();
    // Waitable descriptors take their notifications from a pool for the same reason
    // threads do: one created after a thread exists is invisible to it.
    fd::init();
    file::init();
    // No arguments to pass yet: there is no shell to pass them. `argv[0]` exists
    // because a C program is entitled to read it.
    let mut name = *b"program\0";
    let mut argv: [*mut core::ffi::c_char; 2] =
        [name.as_mut_ptr().cast::<core::ffi::c_char>(), core::ptr::null_mut()];
    // Constructors before `main`: a C program's `.init_array` is empty and never
    // notices, a C++ program's holds every namespace-scope object it declared.
    // SAFETY: the entries are function pointers the compiler emitted for this image.
    unsafe { cxx::run_init_array() };
    // SAFETY: `main` is provided by the program being linked; this is the C ABI.
    let status = unsafe { main(1, argv.as_mut_ptr()) };
    exit_process(status)
}

/// End the process the way every path out of a C program should: flush what was
/// printed without a newline, tell the file server there will be no more requests,
/// then exit.
///
/// A program that skips the goodbye leaves its server blocked in `Recv` forever,
/// and from outside that looks like the kernel failing to shut down rather than
/// like a client that walked away.
#[cfg(not(test))]
fn exit_process(status: c_int) -> ! {
    // Destructors first, while the console and the file server are still usable —
    // a static object's destructor that wants to log has nowhere to log after the
    // flush below.
    cxx::run_atexit();
    stdio::flush();
    file::shutdown();
    sys::exit(status)
}

/// Stop the program because a fortified call was about to write past the end of a
/// buffer whose size the compiler knew.
///
/// This is what `_FORTIFY_SOURCE` buys: the caller passed the destination's size
/// alongside the pointer, so the overflow is detectable *before* it happens rather
/// than being found later as a corrupted neighbour. Continuing after detecting it
/// would throw that away, so this does not return.
#[cfg(not(test))]
pub(crate) fn chk_fail(name: &str) -> ! {
    stdio::write_bytes(b"[libc] ");
    stdio::write_bytes(name.as_bytes());
    stdio::write_bytes(b": buffer overflow detected\n");
    stdio::flush();
    sys::exit(134) // 128 + SIGABRT, as `abort` reports
}

/// The C entry points that belong to no single module.
#[cfg(not(test))]
mod exports {
    use super::{c_int, c_void, stdio, sys, ERRNO, HEAP, HEAP_LOCK};

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub extern "C" fn malloc(size: usize) -> *mut c_void {
        let _guard = HEAP_LOCK.lock();
        // SAFETY: exclusive under the lock every user of `HEAP` takes.
        unsafe { (*core::ptr::addr_of_mut!(HEAP)).alloc(size).cast::<c_void>() }
    }

    /// # Safety
    /// C ABI: `ptr` is null or came from this allocator.
    #[no_mangle]
    pub unsafe extern "C" fn free(ptr: *mut c_void) {
        let _guard = HEAP_LOCK.lock();
        // SAFETY: forwarded from the caller, under the heap lock.
        unsafe { (*core::ptr::addr_of_mut!(HEAP)).free(ptr.cast::<u8>()) };
    }

    #[no_mangle]
    pub extern "C" fn calloc(count: usize, size: usize) -> *mut c_void {
        // The multiplication is where `calloc` earns its existence: a program that
        // computes `count * size` itself can overflow and allocate a small buffer
        // for a large loop. Refusing is the only safe answer.
        let Some(total) = count.checked_mul(size) else {
            return core::ptr::null_mut();
        };
        let p = {
            let _guard = HEAP_LOCK.lock();
            // SAFETY: exclusive under the heap lock.
            unsafe { (*core::ptr::addr_of_mut!(HEAP)).alloc(total) }
        };
        if !p.is_null() {
            // SAFETY: the allocator just gave us `total` writable bytes.
            unsafe { core::ptr::write_bytes(p, 0, total) };
        }
        p.cast::<c_void>()
    }

    /// # Safety
    /// C ABI: `ptr` is null or came from this allocator.
    #[no_mangle]
    pub unsafe extern "C" fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
        let _guard = HEAP_LOCK.lock();
        // SAFETY: forwarded from the caller, under the heap lock.
        unsafe {
            (*core::ptr::addr_of_mut!(HEAP))
                .realloc(ptr.cast::<u8>(), size)
                .cast::<c_void>()
        }
    }

    #[no_mangle]
    pub extern "C" fn aligned_alloc(align: usize, size: usize) -> *mut c_void {
        if !align.is_power_of_two() {
            return core::ptr::null_mut();
        }
        let _guard = HEAP_LOCK.lock();
        // SAFETY: exclusive under the heap lock.
        unsafe {
            (*core::ptr::addr_of_mut!(HEAP))
                .alloc_aligned(size, align)
                .cast::<c_void>()
        }
    }

    /// # Safety
    /// C ABI: `out` is valid for one pointer.
    #[no_mangle]
    pub unsafe extern "C" fn posix_memalign(
        out: *mut *mut c_void,
        align: usize,
        size: usize,
    ) -> c_int {
        let p = aligned_alloc(align, size);
        if p.is_null() {
            return 12; // ENOMEM, the one errno value a caller of this checks
        }
        // SAFETY: forwarded from the caller.
        unsafe { *out = p };
        0
    }

    /// Live allocation statistics, which no C library exposes and every debugging
    /// session wants: bytes and blocks still out. A leak in a program that has no
    /// allocator of its own is otherwise invisible until memory runs out.
    #[no_mangle]
    pub extern "C" fn staros_heap_live(bytes: *mut usize, blocks: *mut usize) {
        let _guard = HEAP_LOCK.lock();
        // SAFETY: read under the heap lock; the caller passes two writable words.
        unsafe {
            let h = &*core::ptr::addr_of!(HEAP);
            if !bytes.is_null() {
                *bytes = h.live_bytes;
            }
            if !blocks.is_null() {
                *blocks = h.live_blocks;
            }
        }
    }

    #[no_mangle]
    pub extern "C" fn exit(status: c_int) -> ! {
        super::exit_process(status)
    }

    #[no_mangle]
    pub extern "C" fn _exit(status: c_int) -> ! {
        sys::exit(status)
    }

    #[no_mangle]
    pub extern "C" fn abort() -> ! {
        stdio::write_bytes(b"[libc] abort()\n");
        stdio::flush();
        sys::exit(134) // 128 + SIGABRT, what a shell would report
    }

    /// `rand`'s state. One word for the process, seeded to 1 as C requires so a
    /// program that never calls `srand` gets the same sequence every run — which is
    /// the property that makes a bug reproducible, and the reason `rand` must never
    /// be used where unpredictability matters.
    static mut RAND_STATE: u32 = 1;

    /// `RAND_MAX` here, and the value `stdlib.h` declares.
    const RAND_MAX: c_int = 0x7fff_ffff;

    /// A linear congruential generator, the constants from Numerical Recipes.
    ///
    /// The low bits of an LCG are famously poor — bit 0 alternates — so the result
    /// is taken from the *top* of the word. A version returning `state % RAND_MAX`
    /// passes every test anyone writes and produces an obviously periodic sequence
    /// in the one place it is visible, which is a die roll in a game.
    #[no_mangle]
    pub extern "C" fn rand() -> c_int {
        // SAFETY: a single word; `rand` is not thread-safe by specification, and
        // `rand_r` is the function that is.
        unsafe {
            RAND_STATE = RAND_STATE.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((RAND_STATE >> 1) & RAND_MAX as u32) as c_int
        }
    }

    #[no_mangle]
    pub extern "C" fn srand(seed: core::ffi::c_uint) {
        // SAFETY: as `rand`.
        unsafe { RAND_STATE = seed };
    }

    /// # Safety
    /// C ABI: `state` points at one writable `unsigned`.
    #[no_mangle]
    pub unsafe extern "C" fn rand_r(state: *mut core::ffi::c_uint) -> c_int {
        if state.is_null() {
            return 0;
        }
        // SAFETY: the caller passes a writable word, as the prototype says.
        unsafe {
            *state = (*state).wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((*state >> 1) & RAND_MAX as u32) as c_int
        }
    }

    /// `quick_exit`: end the process without running `atexit` handlers.
    ///
    /// The distinction from `exit` is the whole point — `quick_exit` is what a
    /// program calls when its own invariants are broken and running destructors
    /// would make things worse. Its own handler list (`at_quick_exit`) is separate,
    /// and empty here because nothing has registered one.
    #[no_mangle]
    pub extern "C" fn quick_exit(status: c_int) -> ! {
        stdio::flush();
        sys::exit(status)
    }

    /// Register a handler for `quick_exit`. Always refuses.
    ///
    /// Refusing rather than accepting-and-forgetting: a handler that is silently
    /// never run is the failure this library exists not to have. C says a non-zero
    /// return means the registration failed, which a caller can act on.
    #[no_mangle]
    pub extern "C" fn at_quick_exit(_handler: Option<extern "C" fn()>) -> c_int {
        -1
    }

    /// `_Exit`: end immediately, no flushing, no handlers. The C name for what
    /// `_exit` does, and the same function.
    #[no_mangle]
    pub extern "C" fn _Exit(status: c_int) -> ! {
        sys::exit(status)
    }

    /// What a failed `assert` calls.
    ///
    /// The name and the four arguments are glibc's, because that is what every
    /// header compiled against a Linux sysroot expands `assert` into — libstdc++'s
    /// included. A tidier signature here would mean patching all of them.
    ///
    /// It prints and ends the process rather than calling `abort`, so the message
    /// and the exit are one path: a report that reached the console followed by a
    /// second failure inside the abort handler is how an assertion comes to look
    /// like a crash with no diagnosis.
    ///
    /// # Safety
    /// C ABI: all four pointers are NUL-terminated C strings, as `assert` builds
    /// them from string literals and `__PRETTY_FUNCTION__`.
    #[no_mangle]
    pub unsafe extern "C" fn __assert_fail(
        expression: *const core::ffi::c_char,
        file: *const core::ffi::c_char,
        line: core::ffi::c_uint,
        function: *const core::ffi::c_char,
    ) -> ! {
        /// Write a C string, or a placeholder when the caller passed null. A
        /// diagnostic that faults while reporting a fault tells nobody anything.
        // SAFETY: forwarded from the caller's contract.
        unsafe fn put(p: *const core::ffi::c_char) {
            if p.is_null() {
                stdio::write_bytes(b"?");
                return;
            }
            let mut n = 0;
            // SAFETY: the caller promised a NUL-terminated string.
            while unsafe { *p.add(n) } != 0 {
                n += 1;
            }
            // SAFETY: `n` bytes were just walked and found to be readable.
            stdio::write_bytes(unsafe { core::slice::from_raw_parts(p.cast::<u8>(), n) });
        }
        stdio::write_bytes(b"[libc] assertion failed: ");
        // SAFETY: forwarded from the caller.
        unsafe { put(expression) };
        stdio::write_bytes(b", at ");
        // SAFETY: forwarded from the caller.
        unsafe { put(file) };
        stdio::write_bytes(b":");
        let mut digits = [0u8; 10];
        let mut i = digits.len();
        let mut value = line;
        loop {
            i -= 1;
            digits[i] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        stdio::write_bytes(&digits[i..]);
        stdio::write_bytes(b" in ");
        // SAFETY: forwarded from the caller.
        unsafe { put(function) };
        stdio::write_bytes(b"\n");
        stdio::flush();
        sys::exit(134) // 128 + SIGABRT, what a shell would report
    }

    /// The stack-protector's failure handler. Programs are not built with
    /// `-fstack-protector` here, but a library that omits this fails to link the
    /// day somebody turns it on, with an error that names no source file.
    #[no_mangle]
    pub extern "C" fn __stack_chk_fail() -> ! {
        stdio::write_bytes(b"[libc] stack smashing detected\n");
        stdio::flush();
        sys::exit(134)
    }

    #[no_mangle]
    pub extern "C" fn __errno_location() -> *mut c_int {
        core::ptr::addr_of_mut!(ERRNO)
    }

    #[no_mangle]
    pub extern "C" fn getpagesize() -> c_int {
        4096
    }

    // `sysconf` used to live here and answered two names. It moved to
    // `crate::proc` with layer 7, where the descriptor limit and the clock tick it
    // now also answers are the same numbers the rest of that module reports.
}

/// Nothing in this library panics deliberately; the lang item has to exist for the
/// bare-metal build regardless. A panic that does happen says so and stops, rather
/// than unwinding into a C caller that has no idea what unwinding is.
#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    stdio::write_bytes(b"[libc] panic in the C library\n");
    stdio::flush();
    sys::exit(101)
}
