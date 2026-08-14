//! `FILE*`: the buffered layer C programs actually use.
//!
//! Layer 4 gave programs `open`/`read`/`close`, which is what the file server
//! speaks. Almost nothing in C calls those directly — `fopen`/`fgets`/`fread` is
//! what source code says, what libstdc++'s `basic_filebuf` calls underneath, and
//! what FreeType wants when it is handed a font path. This module is the adapter,
//! and it is a real one: a `FILE` here owns a descriptor and a buffer, and `fgetc`
//! costs an IPC round trip once per kilobyte rather than once per byte.
//!
//! ## Three streams that are not files
//! `stdin`, `stdout` and `stderr` are `FILE`s whose descriptors are 0, 1 and 2.
//! Writing to the last two goes to the console line buffer in [`crate::stdio`],
//! which is where the atomic-line guarantee lives; reading `stdin` returns end of
//! file, because there is no keyboard on the other side of this library. That is a
//! statement about the system rather than a stub: `inputsrv` delivers key events to
//! a display server, not a byte stream, and inventing one here would give a program
//! a `getchar` that blocks forever.
//!
//! ## What a stream cannot do
//! Writing to a file is refused, because the file server is read-only and says so.
//! `fopen(path, "w")` therefore fails at the *open*, which is where a program checks
//! — rather than at the first `fwrite`, which is where a program does not.

use core::ffi::{c_char, c_int, c_void};

/// How much a stream reads ahead. One page: the same unit the shared buffer
/// underneath is measured in, so a full refill is exactly one round trip.
const BUFFER: usize = 4096;

/// Nothing has been pushed back.
const NO_UNGOT: c_int = -1;

/// C's end-of-file sentinel.
pub(crate) const EOF: c_int = -1;

/// The `FILE` a C program passes around.
///
/// `#[repr(C)]` because programs are entitled to hold `FILE*` and pass it back; the
/// fields are private to this library and no header exposes them, which is exactly
/// how glibc does it.
#[repr(C)]
pub struct File {
    /// The descriptor underneath, or -1 for a closed stream.
    pub(crate) fd: c_int,
    /// End of file has been reached — sticky until `clearerr`, as C requires.
    eof: bool,
    /// An operation failed — likewise sticky.
    error: bool,
    /// Whether this stream owns `fd` and should close it with the stream.
    owns_fd: bool,
    /// One byte pushed back by `ungetc`, or [`NO_UNGOT`].
    ungot: c_int,
    /// Read-ahead buffer: `buf[start..end]` is data read from the file and not yet
    /// handed to the program.
    start: usize,
    end: usize,
    buf: [u8; BUFFER],
}

impl File {
    /// A stream over an existing descriptor.
    const fn over(fd: c_int, owns_fd: bool) -> Self {
        Self {
            fd,
            eof: false,
            error: false,
            owns_fd,
            ungot: NO_UNGOT,
            start: 0,
            end: 0,
            buf: [0; BUFFER],
        }
    }
}

/// The three standard streams. `static mut` rather than `static`, because a stream
/// carries state: an `feof` flag that a `const` in read-only memory could not hold.
static mut STDIN: File = File::over(0, false);
static mut STDOUT: File = File::over(1, false);
static mut STDERR: File = File::over(2, false);

/// Does writing to this stream reach anything?
///
/// True for the console streams and for a null pointer — `fputs(s, 0)` is a bug,
/// but one whose least surprising behaviour is to print. False for a file, because
/// the file server is read-only.
///
/// # Safety
/// `f` is null or a stream from this library.
#[cfg(not(test))]
pub(crate) unsafe fn writable(f: *mut File) -> bool {
    if f.is_null() {
        return true;
    }
    // SAFETY: the caller's contract.
    let fd = unsafe { (*f).fd };
    fd == 1 || fd == 2
}

/// The C entry points.
#[cfg(not(test))]
pub mod exports {
    use super::{c_char, c_int, c_void, File, BUFFER, EOF, NO_UNGOT, STDERR, STDIN, STDOUT};
    use crate::fail;

    const EINVAL: c_int = 22;
    const ENOMEM: c_int = 12;
    const EROFS: c_int = 30;
    const EBADF: c_int = 9;

    // The names C programs use. They are pointers, not structures, so a program may
    // assign to them — which is why they are `static mut` and why `stdout` being
    // reassigned by a caller is that caller's business.
    #[no_mangle]
    pub static mut stdin: *mut File = core::ptr::addr_of_mut!(STDIN);
    #[no_mangle]
    pub static mut stdout: *mut File = core::ptr::addr_of_mut!(STDOUT);
    #[no_mangle]
    pub static mut stderr: *mut File = core::ptr::addr_of_mut!(STDERR);

    /// Is this one of the three streams that live in static storage rather than on
    /// the heap? They must never be `free`d, however hard a program tries.
    fn is_standard(f: *mut File) -> bool {
        core::ptr::eq(f, core::ptr::addr_of_mut!(STDIN))
            || core::ptr::eq(f, core::ptr::addr_of_mut!(STDOUT))
            || core::ptr::eq(f, core::ptr::addr_of_mut!(STDERR))
    }

    /// Does this mode string ask to write?
    ///
    /// Anything but a plain `r` does. `r+` is included: a program that opens for
    /// update and is told yes will discover the truth at the first write, having
    /// already decided the file is writable.
    fn wants_write(mode: &[u8]) -> bool {
        mode.first().is_none_or(|&c| c != b'r') || mode.contains(&b'+')
    }

    /// # Safety
    /// C ABI: both arguments are NUL-terminated strings.
    #[no_mangle]
    pub unsafe extern "C" fn fopen(path: *const c_char, mode: *const c_char) -> *mut File {
        // SAFETY: forwarded from the caller.
        let mode_bytes = unsafe { crate::string::as_bytes(mode) };
        if wants_write(mode_bytes) {
            // Read-only, and the refusal belongs here: a program checks the result
            // of fopen and does not check the result of every fwrite.
            return fail(EROFS, core::ptr::null_mut());
        }
        // SAFETY: forwarded from the caller.
        let fd = crate::file::open(unsafe { crate::string::as_bytes(path) });
        if fd < 0 {
            return fail(2, core::ptr::null_mut()); // ENOENT
        }
        let stream = new_stream(fd, true);
        if stream.is_null() {
            crate::file::exports::close(fd);
            return fail(ENOMEM, core::ptr::null_mut());
        }
        stream
    }

    /// The large-file name, which on a 64-bit target is the same function. Qt's
    /// objects reference this one because glibc's headers redirect to it.
    ///
    /// # Safety
    /// As [`fopen`].
    #[no_mangle]
    pub unsafe extern "C" fn fopen64(path: *const c_char, mode: *const c_char) -> *mut File {
        // SAFETY: forwarded from the caller.
        unsafe { fopen(path, mode) }
    }

    /// # Safety
    /// C ABI: `mode` is NUL-terminated; `fd` is an open descriptor.
    #[no_mangle]
    pub unsafe extern "C" fn fdopen(fd: c_int, mode: *const c_char) -> *mut File {
        // SAFETY: forwarded from the caller.
        let mode_bytes = unsafe { crate::string::as_bytes(mode) };
        if wants_write(mode_bytes) && fd > 2 {
            return fail(EROFS, core::ptr::null_mut());
        }
        // `fdopen` does not take ownership in the sense `fopen` does: C says
        // `fclose` closes the descriptor, so it does here too.
        new_stream(fd, true)
    }

    /// Allocate a stream on the heap.
    fn new_stream(fd: c_int, owns_fd: bool) -> *mut File {
        let p = crate::heap_alloc(core::mem::size_of::<File>(), 16).cast::<File>();
        if p.is_null() {
            return p;
        }
        // SAFETY: `p` is a fresh allocation of exactly one `File`.
        unsafe { p.write(File::over(fd, owns_fd)) };
        p
    }

    /// # Safety
    /// C ABI: `f` came from `fopen` or is one of the standard streams.
    #[no_mangle]
    pub unsafe extern "C" fn fclose(f: *mut File) -> c_int {
        if f.is_null() {
            return fail(EINVAL, EOF);
        }
        // SAFETY: the caller's contract.
        let stream = unsafe { &mut *f };
        if stream.fd <= 2 {
            crate::stdio::flush();
        }
        let fd = stream.fd;
        let owns = stream.owns_fd;
        stream.fd = -1;
        if owns && fd > 2 {
            crate::file::exports::close(fd);
        }
        if !is_standard(f) {
            // SAFETY: the allocation came from `new_stream`, which used the same
            // heap `free` releases to.
            unsafe { crate::exports::free(f.cast::<c_void>()) };
        }
        0
    }

    /// Fill the buffer from the descriptor. Returns how many bytes are now
    /// available, and sets `eof`/`error` on the way.
    fn refill(stream: &mut File) -> usize {
        if stream.start < stream.end {
            return stream.end - stream.start;
        }
        if stream.fd < 0 {
            stream.error = true;
            return 0;
        }
        stream.start = 0;
        stream.end = 0;
        // SAFETY: the buffer is `BUFFER` bytes and belongs to this stream.
        let got = unsafe {
            crate::file::exports::read(
                stream.fd,
                stream.buf.as_mut_ptr().cast::<c_void>(),
                BUFFER,
            )
        };
        if got < 0 {
            stream.error = true;
            return 0;
        }
        if got == 0 {
            stream.eof = true;
            return 0;
        }
        stream.end = got as usize;
        got as usize
    }

    /// Take up to `dst.len()` bytes out of the stream.
    fn take(stream: &mut File, dst: &mut [u8]) -> usize {
        let mut done = 0;
        // A pushed-back byte comes first and is not in the buffer, which is the
        // whole point of `ungetc` — it must survive a refill.
        if stream.ungot != NO_UNGOT && !dst.is_empty() {
            dst[0] = stream.ungot as u8;
            stream.ungot = NO_UNGOT;
            done = 1;
        }
        while done < dst.len() {
            let available = refill(stream);
            if available == 0 {
                break;
            }
            let n = available.min(dst.len() - done);
            dst[done..done + n].copy_from_slice(&stream.buf[stream.start..stream.start + n]);
            stream.start += n;
            done += n;
        }
        done
    }

    /// # Safety
    /// C ABI: `ptr` is valid for `size * count` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn fread(
        ptr: *mut c_void,
        size: usize,
        count: usize,
        f: *mut File,
    ) -> usize {
        if f.is_null() || ptr.is_null() || size == 0 || count == 0 {
            return 0;
        }
        // SAFETY: the caller's contract.
        let stream = unsafe { &mut *f };
        let Some(total) = size.checked_mul(count) else {
            return fail(EINVAL, 0);
        };
        // SAFETY: the caller promised `total` writable bytes.
        let dst = unsafe { core::slice::from_raw_parts_mut(ptr.cast::<u8>(), total) };
        let got = take(stream, dst);
        // C counts whole *items*, not bytes: a partial item at the end is not
        // reported, which is why `fread` of a struct array can return less than the
        // bytes it actually moved.
        got / size
    }

    /// # Safety
    /// C ABI: `ptr` is valid for `size * count` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn fwrite(
        ptr: *const c_void,
        size: usize,
        count: usize,
        f: *mut File,
    ) -> usize {
        if f.is_null() || ptr.is_null() {
            return 0;
        }
        // SAFETY: the caller's contract.
        let stream = unsafe { &mut *f };
        let Some(total) = size.checked_mul(count) else {
            return fail(EINVAL, 0);
        };
        // SAFETY: the caller promised `total` readable bytes.
        let src = unsafe { core::slice::from_raw_parts(ptr.cast::<u8>(), total) };
        if stream.fd == 1 || stream.fd == 2 {
            crate::stdio::write_bytes(src);
            return count;
        }
        stream.error = true;
        fail(EROFS, 0)
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn fgetc(f: *mut File) -> c_int {
        if f.is_null() {
            return EOF;
        }
        // SAFETY: the caller's contract.
        let stream = unsafe { &mut *f };
        let mut byte = [0u8; 1];
        if take(stream, &mut byte) == 1 {
            c_int::from(byte[0])
        } else {
            EOF
        }
    }

    /// `getc` is `fgetc` — the difference in C is that one may be a macro.
    ///
    /// # Safety
    /// As [`fgetc`].
    #[no_mangle]
    pub unsafe extern "C" fn getc(f: *mut File) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { fgetc(f) }
    }

    #[no_mangle]
    pub extern "C" fn getchar() -> c_int {
        // SAFETY: `stdin` is a valid stream.
        unsafe { fgetc(core::ptr::addr_of_mut!(STDIN)) }
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn ungetc(c: c_int, f: *mut File) -> c_int {
        if f.is_null() || c == EOF {
            return EOF;
        }
        // SAFETY: the caller's contract.
        let stream = unsafe { &mut *f };
        if stream.ungot != NO_UNGOT {
            // C guarantees one byte of pushback and no more. Refusing the second is
            // better than silently dropping the first, which a parser would then
            // read as a missing character.
            return EOF;
        }
        stream.ungot = c & 0xff;
        // Pushing a byte back un-ends the file, which is what lets a parser peek at
        // the last character and then continue.
        stream.eof = false;
        c & 0xff
    }

    /// # Safety
    /// C ABI: `buf` is valid for `size` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn fgets(buf: *mut c_char, size: c_int, f: *mut File) -> *mut c_char {
        if buf.is_null() || size <= 0 || f.is_null() {
            return core::ptr::null_mut();
        }
        // SAFETY: the caller's contract.
        let stream = unsafe { &mut *f };
        let room = size as usize - 1; // C reserves one byte for the NUL
        let mut n = 0;
        while n < room {
            let mut byte = [0u8; 1];
            if take(stream, &mut byte) == 0 {
                break;
            }
            // SAFETY: `n < room < size`, and the caller promised `size` bytes.
            unsafe { *buf.add(n) = byte[0] as c_char };
            n += 1;
            if byte[0] == b'\n' {
                // The newline is kept, which is the difference from `gets` and from
                // every hand-written line reader that strips it and loses the
                // distinction between a last line with and without one.
                break;
            }
        }
        if n == 0 {
            return core::ptr::null_mut();
        }
        // SAFETY: `n <= room`, so this is inside the buffer.
        unsafe { *buf.add(n) = 0 };
        buf
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn feof(f: *mut File) -> c_int {
        if f.is_null() {
            return 0;
        }
        // SAFETY: the caller's contract.
        c_int::from(unsafe { (*f).eof })
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn ferror(f: *mut File) -> c_int {
        if f.is_null() {
            return 0;
        }
        // SAFETY: the caller's contract.
        c_int::from(unsafe { (*f).error })
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn clearerr(f: *mut File) {
        if f.is_null() {
            return;
        }
        // SAFETY: the caller's contract.
        unsafe {
            (*f).eof = false;
            (*f).error = false;
        }
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn fileno(f: *mut File) -> c_int {
        if f.is_null() {
            return fail(EBADF, -1);
        }
        // SAFETY: the caller's contract.
        unsafe { (*f).fd }
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn fseek(f: *mut File, offset: i64, whence: c_int) -> c_int {
        // SAFETY: forwarded from the caller.
        if unsafe { fseeko(f, offset, whence) } < 0 {
            -1
        } else {
            0
        }
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn fseeko(f: *mut File, offset: i64, whence: c_int) -> i64 {
        if f.is_null() {
            return fail(EINVAL, -1);
        }
        // SAFETY: the caller's contract.
        let stream = unsafe { &mut *f };
        // A seek relative to the *stream* position must account for what is sitting
        // in the buffer: the descriptor is further along than the program is. This
        // is the classic buffered-seek bug, and it shows up as a file read twice
        // from the wrong place rather than as an error.
        let buffered = (stream.end - stream.start) as i64
            + i64::from(stream.ungot != NO_UNGOT);
        let adjusted = if whence == 1 { offset - buffered } else { offset };
        let position = crate::file::seek(stream.fd, adjusted, whence);
        if position < 0 {
            return fail(EINVAL, -1);
        }
        stream.start = 0;
        stream.end = 0;
        stream.ungot = NO_UNGOT;
        stream.eof = false;
        position
    }

    /// `fpos_t`: an opaque position. C says it need not be an integer — a
    /// multibyte-encoded stream would need shift state alongside the offset — so it
    /// is a struct here rather than a `long`, which keeps a caller from doing
    /// arithmetic on it that would stop working the day it needs to be more.
    #[repr(C)]
    pub struct FPos {
        offset: i64,
    }

    /// # Safety
    /// C ABI: `pos` points at one writable `fpos_t`.
    #[no_mangle]
    pub unsafe extern "C" fn fgetpos(f: *mut File, pos: *mut FPos) -> c_int {
        if pos.is_null() {
            return fail(EINVAL, -1);
        }
        // SAFETY: forwarded from the caller.
        let at = unsafe { ftello(f) };
        if at < 0 {
            return -1;
        }
        // SAFETY: the caller passes a writable `fpos_t`.
        unsafe { (*pos).offset = at };
        0
    }

    /// # Safety
    /// C ABI: `pos` points at one `fpos_t` a previous `fgetpos` filled in.
    #[no_mangle]
    pub unsafe extern "C" fn fsetpos(f: *mut File, pos: *const FPos) -> c_int {
        if pos.is_null() {
            return fail(EINVAL, -1);
        }
        // SAFETY: forwarded from the caller.
        let want = unsafe { (*pos).offset };
        // SAFETY: as above; 0 is SEEK_SET.
        if unsafe { fseeko(f, want, 0) } < 0 {
            -1
        } else {
            0
        }
    }

    /// # Safety
    /// As [`fseeko`].
    #[no_mangle]
    pub unsafe extern "C" fn fseeko64(f: *mut File, offset: i64, whence: c_int) -> i64 {
        // SAFETY: forwarded from the caller.
        unsafe { fseeko(f, offset, whence) }
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn ftell(f: *mut File) -> i64 {
        // SAFETY: forwarded from the caller.
        unsafe { ftello(f) }
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn ftello(f: *mut File) -> i64 {
        if f.is_null() {
            return fail(EINVAL, -1);
        }
        // SAFETY: the caller's contract.
        let stream = unsafe { &mut *f };
        let position = crate::file::seek(stream.fd, 0, 1); // SEEK_CUR
        if position < 0 {
            return fail(EINVAL, -1);
        }
        // Where the *program* is, not where the descriptor is: subtract the
        // read-ahead that has not been handed over yet.
        let buffered = (stream.end - stream.start) as i64
            + i64::from(stream.ungot != NO_UNGOT);
        position - buffered
    }

    /// # Safety
    /// As [`ftello`].
    #[no_mangle]
    pub unsafe extern "C" fn ftello64(f: *mut File) -> i64 {
        // SAFETY: forwarded from the caller.
        unsafe { ftello(f) }
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn rewind(f: *mut File) {
        // SAFETY: forwarded from the caller.
        unsafe {
            fseeko(f, 0, 0);
            clearerr(f);
        }
    }

    /// `setvbuf`, which accepts and changes nothing.
    ///
    /// The buffering here is fixed: a page of read-ahead per stream and line
    /// buffering on the console. Returning success is honest for the one thing
    /// programs use this for — asking for *more* buffering than the default — and
    /// the one case where it is not, `_IONBF` on a stream a program then expects to
    /// interleave with another process's output, cannot happen: each console write
    /// is already one syscall.
    ///
    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn setvbuf(
        _f: *mut File,
        _buf: *mut c_char,
        _mode: c_int,
        _size: usize,
    ) -> c_int {
        0
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn setbuf(_f: *mut File, _buf: *mut c_char) {}

    /// `getline`, which allocates. It is not in the contract, but `getdelim` and it
    /// are how every C program written this century reads a line of unknown length,
    /// and a `FILE*` layer without them invites `fgets` loops with fixed buffers.
    ///
    /// # Safety
    /// C ABI: `line` and `capacity` are a matched pair, either null/0 or a previous
    /// result of this function.
    #[no_mangle]
    pub unsafe extern "C" fn getdelim(
        line: *mut *mut c_char,
        capacity: *mut usize,
        delimiter: c_int,
        f: *mut File,
    ) -> isize {
        if line.is_null() || capacity.is_null() || f.is_null() {
            return fail(EINVAL, -1);
        }
        // SAFETY: the caller's contract.
        let stream = unsafe { &mut *f };
        // SAFETY: as above.
        let (mut buf, mut cap) = unsafe { (*line, *capacity) };
        let mut n = 0usize;
        loop {
            if n + 1 >= cap {
                let want = if cap == 0 { 128 } else { cap * 2 };
                // SAFETY: `buf` is null or came from this allocator.
                let grown = unsafe {
                    crate::exports::realloc(buf.cast::<c_void>(), want).cast::<c_char>()
                };
                if grown.is_null() {
                    return fail(ENOMEM, -1);
                }
                buf = grown;
                cap = want;
                // Publish every growth immediately: a failure after this point must not
                // leave the caller holding a pointer this function has freed.
                // SAFETY: the caller's out-parameters.
                unsafe {
                    *line = buf;
                    *capacity = cap;
                }
            }
            let mut byte = [0u8; 1];
            if take(stream, &mut byte) == 0 {
                break;
            }
            // SAFETY: `n + 1 < cap`, checked above.
            unsafe { *buf.add(n) = byte[0] as c_char };
            n += 1;
            if c_int::from(byte[0]) == delimiter {
                break;
            }
        }
        if n == 0 {
            return -1; // end of file with nothing read
        }
        // SAFETY: `n < cap`.
        unsafe { *buf.add(n) = 0 };
        n as isize
    }

    /// # Safety
    /// As [`getdelim`].
    #[no_mangle]
    pub unsafe extern "C" fn getline(
        line: *mut *mut c_char,
        capacity: *mut usize,
        f: *mut File,
    ) -> isize {
        // SAFETY: forwarded from the caller.
        unsafe { getdelim(line, capacity, c_int::from(b'\n'), f) }
    }

    /// `tmpfile` and `freopen`, refused rather than faked: there is nowhere to
    /// write. A program that gets null from these takes its error path, which is
    /// what a program that got a stream it could not write to would never do.
    #[no_mangle]
    pub extern "C" fn tmpfile() -> *mut File {
        fail(EROFS, core::ptr::null_mut())
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn freopen(
        _path: *const c_char,
        _mode: *const c_char,
        _f: *mut File,
    ) -> *mut File {
        fail(EROFS, core::ptr::null_mut())
    }
}
