//! The `printf` family and the standard streams, on top of [`crate::fmt`].
//!
//! There is no terminal and no pipe: `stdout` and `stderr` both end at
//! `DebugWrite`, the kernel's console syscall. What matters is *when* the syscall
//! happens. One call is one atomic line as far as the console lock is concerned, so
//! output is buffered until a newline: a program that printf's a line in five calls
//! would otherwise have another core's output spliced through the middle of it,
//! which is how the display server's log used to read.

// The C-ABI half of this module is compiled only for the target; on the host these
// names would be unused, and `printf` in particular would collide with the system's.
#[cfg(not(test))]
use core::ffi::{c_char, c_int, c_void};

#[cfg(not(test))]
use crate::fmt::{self, Args, BufSink, Length};
use crate::fmt::Sink;
use crate::sys;

/// The line buffer. Large enough for any line a program is likely to print in one
/// go; a longer line is flushed in pieces rather than truncated.
const LINE: usize = 1024;

struct Stream {
    buf: [u8; LINE],
    len: usize,
}

static mut OUT: Stream = Stream { buf: [0; LINE], len: 0 };

/// The `FILE` a C program passes around. It carries nothing: both streams go to the
/// same place, and the only thing this library needs from a `FILE*` is to tell them
/// apart — which it does not yet need to, because neither is redirectable.
#[repr(C)]
pub struct File {
    _private: u8,
}

#[no_mangle]
pub static mut stdout: *mut File = core::ptr::addr_of!(STDOUT) as *mut File;
#[no_mangle]
pub static mut stderr: *mut File = core::ptr::addr_of!(STDERR) as *mut File;

static STDOUT: File = File { _private: 1 };
static STDERR: File = File { _private: 2 };

/// Append bytes to the line buffer, flushing at every newline and whenever the
/// buffer is full.
pub(crate) fn write_bytes(bytes: &[u8]) {
    // SAFETY: single-threaded until layer 5; see `crate::file::FILES`.
    let out = unsafe { &mut *core::ptr::addr_of_mut!(OUT) };
    for &b in bytes {
        out.buf[out.len] = b;
        out.len += 1;
        if b == b'\n' || out.len == LINE {
            sys::debug_write(&out.buf[..out.len]);
            out.len = 0;
        }
    }
}

/// Push whatever is buffered, whether or not it ends in a newline.
pub(crate) fn flush() {
    // SAFETY: as `write_bytes`.
    let out = unsafe { &mut *core::ptr::addr_of_mut!(OUT) };
    if out.len > 0 {
        sys::debug_write(&out.buf[..out.len]);
        out.len = 0;
    }
}

/// A sink that writes through the line buffer.
struct ConsoleSink;

impl Sink for ConsoleSink {
    fn push(&mut self, byte: u8) {
        write_bytes(&[byte]);
    }
}

/// The variadic argument source.
///
/// The narrowing lives here rather than in the engine because it is an ABI
/// property: C promotes everything smaller than `int` before the call, so `%hhd`
/// must *read* an `int` and then truncate. Reading an `i8` from the list would
/// misalign every argument after it.
#[cfg(not(test))]
struct VaArgs<'a>(core::ffi::VaList<'a>);

#[cfg(not(test))]
impl Args for VaArgs<'_> {
    fn int(&mut self, len: Length) -> i64 {
        // SAFETY: the format string promised an argument of this class; that is the
        // contract every `printf` in C relies on.
        let raw = unsafe {
            match len {
                Length::Char | Length::Short | Length::Int => i64::from(self.0.next_arg::<i32>()),
                Length::Long | Length::LongLong | Length::Size => self.0.next_arg::<i64>(),
            }
        };
        fmt::narrow_signed(raw, len)
    }

    fn uint(&mut self, len: Length) -> u64 {
        // SAFETY: as `int`.
        let raw = unsafe {
            match len {
                Length::Char | Length::Short | Length::Int => u64::from(self.0.next_arg::<u32>()),
                Length::Long | Length::LongLong | Length::Size => self.0.next_arg::<u64>(),
            }
        };
        fmt::narrow_unsigned(raw, len)
    }

    fn ptr(&mut self) -> *const c_void {
        // SAFETY: as `int`.
        unsafe { self.0.next_arg::<*const c_void>() }
    }

    fn double(&mut self) -> f64 {
        // SAFETY: as `int`. C promotes `float` to `double` in a variadic call, so
        // there is only one floating-point case to read.
        unsafe { self.0.next_arg::<f64>() }
    }
}

/// The C entry points.
#[cfg(not(test))]
pub mod exports {
    use super::{c_char, c_int, fmt, write_bytes, BufSink, ConsoleSink, File, VaArgs};

    /// # Safety
    /// C ABI: `fmt` is a NUL-terminated format string whose conversions match the
    /// arguments that follow.
    #[no_mangle]
    pub unsafe extern "C" fn printf(format: *const c_char, args: ...) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe {
            let bytes = crate::string::as_bytes(format);
            let mut source = VaArgs(args);
            fmt::format(&mut ConsoleSink, bytes, &mut source) as c_int
        }
    }

    /// # Safety
    /// As [`printf`]; the stream is ignored because both go to the console.
    #[no_mangle]
    pub unsafe extern "C" fn fprintf(_stream: *mut File, format: *const c_char, args: ...) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe {
            let bytes = crate::string::as_bytes(format);
            let mut source = VaArgs(args);
            fmt::format(&mut ConsoleSink, bytes, &mut source) as c_int
        }
    }

    /// # Safety
    /// C ABI: `buf` is valid for `size` bytes; `format` as [`printf`].
    #[no_mangle]
    pub unsafe extern "C" fn snprintf(
        buf: *mut c_char,
        size: usize,
        format: *const c_char,
        args: ...
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe {
            let bytes = crate::string::as_bytes(format);
            let out = core::slice::from_raw_parts_mut(buf.cast::<u8>(), size);
            let mut sink = BufSink { buf: out, written: 0 };
            let mut source = VaArgs(args);
            let n = fmt::format(&mut sink, bytes, &mut source);
            // C guarantees termination when there is any room at all, and returns
            // the length it *would* have written — callers size buffers from that.
            if size > 0 {
                let last = n.min(size - 1);
                *buf.add(last) = 0;
            }
            n as c_int
        }
    }

    /// # Safety
    /// C ABI: `buf` must be large enough for the result. (It is in the contract
    /// because Qt calls it; the bounded form is always the better choice.)
    #[no_mangle]
    pub unsafe extern "C" fn sprintf(buf: *mut c_char, format: *const c_char, args: ...) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe {
            let bytes = crate::string::as_bytes(format);
            let out = core::slice::from_raw_parts_mut(buf.cast::<u8>(), usize::MAX / 2);
            let mut sink = BufSink { buf: out, written: 0 };
            let mut source = VaArgs(args);
            let n = fmt::format(&mut sink, bytes, &mut source);
            *buf.add(n) = 0;
            n as c_int
        }
    }

    /// # Safety
    /// C ABI: NUL-terminated string.
    #[no_mangle]
    pub unsafe extern "C" fn puts(s: *const c_char) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe {
            write_bytes(crate::string::as_bytes(s));
        }
        write_bytes(b"\n");
        0
    }

    /// # Safety
    /// C ABI: NUL-terminated string.
    #[no_mangle]
    pub unsafe extern "C" fn fputs(s: *const c_char, _stream: *mut File) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe {
            write_bytes(crate::string::as_bytes(s));
        }
        0
    }

    #[no_mangle]
    pub extern "C" fn fputc(c: c_int, _stream: *mut File) -> c_int {
        write_bytes(&[c as u8]);
        c
    }

    #[no_mangle]
    pub extern "C" fn putchar(c: c_int) -> c_int {
        write_bytes(&[c as u8]);
        c
    }

    /// # Safety
    /// C ABI: `ptr` is valid for `size * count` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn fwrite(
        ptr: *const core::ffi::c_void,
        size: usize,
        count: usize,
        _stream: *mut File,
    ) -> usize {
        let total = size.saturating_mul(count);
        // SAFETY: forwarded from the caller.
        write_bytes(unsafe { core::slice::from_raw_parts(ptr.cast::<u8>(), total) });
        count
    }

    #[no_mangle]
    pub extern "C" fn fflush(_stream: *mut File) -> c_int {
        super::flush();
        0
    }

    /// # Safety
    /// C ABI: NUL-terminated string or null.
    #[no_mangle]
    pub unsafe extern "C" fn perror(s: *const c_char) {
        if !s.is_null() {
            // SAFETY: forwarded from the caller.
            unsafe { write_bytes(crate::string::as_bytes(s)) };
            write_bytes(b": ");
        }
        // There is one error so far: whatever the file server refused. Printing a
        // number nobody can decode would be worse than saying so.
        write_bytes(b"operation failed\n");
    }
}
