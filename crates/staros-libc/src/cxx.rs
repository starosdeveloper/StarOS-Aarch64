//! The C-library half of the C++ runtime: `atexit` and the start-up
//! constructors.
//!
//! Neither of these is a C++ idea, which is exactly why they live here. A static
//! object with a destructor compiles into a call to `__cxa_atexit`, and the
//! destructors have to run when the *process* exits — and `exit` belongs to the C
//! library. Putting the table on the C++ side would mean the C runtime calling into
//! the C++ one, a link dependency in the wrong direction that only weak symbols can
//! untangle.
//!
//! The other half is `.init_array`: the list of functions the compiler emits for
//! objects constructed before `main`. A C program has an empty one and never
//! notices; a C++ program that does not run it starts with its globals
//! unconstructed, which fails somewhere far from the cause.

use core::ffi::c_void;

use crate::lock::Spin;

/// One registered destructor.
#[derive(Clone, Copy)]
struct AtExit {
    function: Option<unsafe extern "C" fn(*mut c_void)>,
    argument: *mut c_void,
}

/// Enough for a program's static objects. Fixed, because registration happens
/// while those very objects are being constructed — an allocation here would run
/// the allocator from inside a constructor that may itself be the allocator's.
const MAX_ATEXIT: usize = 64;

static mut ATEXIT: [AtExit; MAX_ATEXIT] =
    [AtExit { function: None, argument: core::ptr::null_mut() }; MAX_ATEXIT];
static mut ATEXIT_COUNT: usize = 0;
static ATEXIT_LOCK: Spin = Spin::new();

extern "C" {
    /// The constructors, bracketed by `services/init/boot/image.ld`.
    static __init_array_start: c_void;
    static __init_array_end: c_void;
}

/// Run everything in `.init_array`, in order. Called from `_start` before `main`.
///
/// # Safety
/// The entries are function pointers the compiler emitted for this image.
pub(crate) unsafe fn run_init_array() {
    let start = core::ptr::addr_of!(__init_array_start) as usize;
    let end = core::ptr::addr_of!(__init_array_end) as usize;
    let mut at = start;
    while at + core::mem::size_of::<usize>() <= end {
        // SAFETY: the linker script guarantees this range holds function pointers.
        let function = unsafe { (at as *const Option<extern "C" fn()>).read() };
        if let Some(f) = function {
            f();
        }
        at += core::mem::size_of::<usize>();
    }
}

/// Run the registered destructors, most recent first. Called from the exit path.
pub(crate) fn run_atexit() {
    loop {
        let entry = {
            let _guard = ATEXIT_LOCK.lock();
            // SAFETY: exclusive under the lock.
            unsafe {
                let count = *core::ptr::addr_of!(ATEXIT_COUNT);
                if count == 0 {
                    return;
                }
                *core::ptr::addr_of_mut!(ATEXIT_COUNT) = count - 1;
                (*core::ptr::addr_of!(ATEXIT))[count - 1]
            }
        };
        // Called *outside* the lock: a destructor is allowed to register another
        // one, and a lock held across it would deadlock on the honest case.
        if let Some(function) = entry.function {
            // SAFETY: the pointer came from a `__cxa_atexit` call, with its
            // argument.
            unsafe { function(entry.argument) };
        }
    }
}

/// The C entry points.
#[cfg(not(test))]
pub mod exports {
    use super::{c_void, AtExit, ATEXIT, ATEXIT_COUNT, ATEXIT_LOCK, MAX_ATEXIT};
    use core::ffi::c_int;

    /// # Safety
    /// C ABI: `function` is called with `argument` at exit.
    #[no_mangle]
    pub unsafe extern "C" fn __cxa_atexit(
        function: Option<unsafe extern "C" fn(*mut c_void)>,
        argument: *mut c_void,
        _dso: *mut c_void,
    ) -> c_int {
        let _guard = ATEXIT_LOCK.lock();
        // SAFETY: exclusive under the lock.
        unsafe {
            let count = *core::ptr::addr_of!(ATEXIT_COUNT);
            if count >= MAX_ATEXIT {
                // Refusing is what makes a full table visible: the caller turns it
                // into a failed construction rather than a destructor that silently
                // never runs.
                return -1;
            }
            (*core::ptr::addr_of_mut!(ATEXIT))[count] = AtExit { function, argument };
            *core::ptr::addr_of_mut!(ATEXIT_COUNT) = count + 1;
        }
        0
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn atexit(function: Option<extern "C" fn()>) -> c_int {
        // The argument-less form is the same table with a null argument; C++'s form
        // is the general one, so there is one implementation rather than two lists
        // that could run in the wrong order relative to each other.
        // SAFETY: the transmute is between two function pointers with compatible
        // calling conventions; the extra argument is never read by the callee.
        let function = function.map(|f| unsafe {
            core::mem::transmute::<extern "C" fn(), unsafe extern "C" fn(*mut c_void)>(f)
        });
        // SAFETY: forwarded.
        unsafe { __cxa_atexit(function, core::ptr::null_mut(), core::ptr::null_mut()) }
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn __cxa_finalize(_dso: *mut c_void) {
        super::run_atexit();
    }
}
