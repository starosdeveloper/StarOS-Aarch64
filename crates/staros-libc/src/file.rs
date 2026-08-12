//! Layer 4 of the contract: files, over the file server.
//!
//! `open`/`read`/`lseek`/`fstat`/`close` are IPC to `services/fssrv` and nothing
//! else — this process holds no storage capability, no archive, and no mapping of
//! one. What a C program calls a file descriptor is an index into the small table
//! here; what the server calls a handle is a number only it can interpret.
//!
//! Two things stay on this side of the boundary on purpose:
//!
//! * **The file offset.** The protocol's `Read` takes an absolute offset, which
//!   makes the server stateless per request and `lseek` a local arithmetic
//!   operation. A server that tracked a cursor per handle would have to care which
//!   client, and which thread of which client, was asking.
//! * **The bounce buffer.** Every transfer goes through one shared page this
//!   process created and delegated, so a `read` of 100 KiB is a loop, not a
//!   promise the server has to keep about memory it does not own.

use core::ffi::c_int;
#[cfg(not(test))]
use core::ffi::c_char;

use staros_abi::fsproto::{
    ERR_NO_FILE, MAX_PATH, TAG_BYE, TAG_CLOSE, TAG_ERROR, TAG_OPEN, TAG_READ, TAG_STAT,
};

use crate::sys::{self, Message};

/// The endpoints the kernel grants a file-server client, in the order it installs
/// them: send on the request endpoint, receive on the reply endpoint.
const EP_REQUEST: u64 = 1;
const EP_REPLY: u64 = 2;

/// The connection to the file server. Descriptors themselves live in
/// [`crate::fd`]: a file is one kind of descriptor among several now, and one table
/// is what lets `poll` and `dup2` see all of them.
struct Files {
    /// The shared buffer every request travels through: capability and mapping.
    buffer_cap: u32,
    buffer: *mut u8,
    buffer_len: usize,
    ready: bool,
}

/// The file state, and the lock every entry point below takes.
///
/// The lock covers the *whole* operation, IPC included, and that is deliberate:
/// there is one shared buffer and one reply endpoint between this process and its
/// file server, so two threads reading at once would overwrite each other's data
/// and could take each other's replies. Serialising file I/O per process is the
/// cost of that arrangement, and it is written down here rather than discovered.
static mut FILES: Files = Files {
    buffer_cap: 0,
    buffer: core::ptr::null_mut(),
    buffer_len: 0,
    ready: false,
};

/// See [`FILES`].
static FILES_LOCK: crate::lock::Spin = crate::lock::Spin::new();

/// Bytes in a page — the unit the shared buffer is measured in.
const PAGE: usize = 4096;

/// Prepare the shared buffer. Called from the C runtime start-up, before `main`.
///
/// A program with no file-server capability still starts: `open` then fails with
/// "no such file", which is the truth from where it sits. Refusing to start would
/// make every program that merely *links* the library depend on a service it may
/// not use.
pub(crate) fn init() {
    // SAFETY: single-threaded start-up, before `main` runs.
    let files = unsafe { &mut *core::ptr::addr_of_mut!(FILES) };
    let Some(cap) = sys::create_shared(1) else {
        return;
    };
    let Some(buffer) = sys::map_shared(cap) else {
        return;
    };
    files.buffer_cap = cap;
    files.buffer = buffer;
    files.buffer_len = PAGE;
    files.ready = true;
}

/// Tell the file server this process is done, so it can report and exit.
///
/// Called from the C runtime's exit path. Without it the server sits in `Recv`
/// forever and the machine never powers off — which is not a hang in the server but
/// a client that walked away without saying anything, and it looks exactly like a
/// kernel bug from the outside.
pub(crate) fn shutdown() {
    // SAFETY: single-threaded; see `FILES`.
    let files = unsafe { &*core::ptr::addr_of!(FILES) };
    if !files.ready {
        return;
    }
    let mut msg = Message::new();
    msg.tag = TAG_BYE;
    sys::send(EP_REQUEST, &msg);
}

/// Send one request and wait for the reply.
fn request(tag: u64, words: [u64; 3], with_buffer: bool) -> Option<Message> {
    // SAFETY: single-threaded; see `FILES`.
    let files = unsafe { &*core::ptr::addr_of!(FILES) };
    if !files.ready {
        return None;
    }
    let mut msg = Message::new();
    msg.tag = tag;
    msg.words[0] = words[0];
    msg.words[1] = words[1];
    msg.words[2] = words[2];
    msg.cap = if with_buffer { files.buffer_cap } else { 0 };
    if sys::send(EP_REQUEST, &msg) < 0 {
        return None;
    }
    sys::recv(EP_REPLY)
}

/// Copy a path into the shared buffer, refusing one that would not fit.
fn put_path(path: &[u8]) -> Option<usize> {
    if path.is_empty() || path.len() > MAX_PATH {
        return None;
    }
    // SAFETY: single-threaded; the buffer is a full page we mapped.
    let files = unsafe { &*core::ptr::addr_of!(FILES) };
    if !files.ready || path.len() > files.buffer_len {
        return None;
    }
    // SAFETY: `path.len()` is inside the mapped page.
    unsafe { core::ptr::copy_nonoverlapping(path.as_ptr(), files.buffer, path.len()) };
    Some(path.len())
}

/// `open`, without the flags: everything here is read-only.
pub(crate) fn open(path: &[u8]) -> c_int {
    let _guard = FILES_LOCK.lock();
    let Some(len) = put_path(path) else {
        return -1;
    };
    let Some(reply) = request(TAG_OPEN, [len as u64, 0, 0], true) else {
        return -1;
    };
    if reply.tag == TAG_ERROR {
        return -1;
    }
    let fd = crate::fd::install_file(reply.words[0], reply.words[1]);
    if fd < 0 {
        // Out of descriptors: the handle the server just opened would leak, so
        // close it before failing. A libc that forgets this runs a server out of
        // handles by failing.
        let _ = request(TAG_CLOSE, [reply.words[0], 0, 0], false);
        return -1;
    }
    fd
}

/// `read`: fill `dst` from the descriptor's cursor, and advance it.
pub(crate) fn read(fd: c_int, dst: &mut [u8]) -> isize {
    let _guard = FILES_LOCK.lock();
    let Some(crate::fd::Kind::File { handle, mut offset, .. }) = crate::fd::get(fd) else {
        return -1;
    };
    // SAFETY: read under the file lock.
    let files = unsafe { &*core::ptr::addr_of!(FILES) };
    let mut done = 0;
    while done < dst.len() {
        let want = (dst.len() - done).min(files.buffer_len);
        let Some(reply) = request(TAG_READ, [handle, offset, want as u64], true) else {
            break;
        };
        if reply.tag == TAG_ERROR {
            break;
        }
        let got = reply.words[0] as usize;
        if got == 0 {
            break; // end of file
        }
        // SAFETY: the server wrote `got` bytes into the page we mapped, and `got`
        // is bounded by the size we asked for and by the page itself.
        unsafe { core::ptr::copy_nonoverlapping(files.buffer, dst.as_mut_ptr().add(done), got) };
        done += got;
        offset += got as u64;
    }
    crate::fd::set_offset(fd, offset);
    if done == 0 && !dst.is_empty() {
        // Nothing read: end of file is zero, and a refusal is an error. The
        // difference is what a caller loops on.
        return 0;
    }
    done as isize
}

/// `lseek`, with the three C whences.
pub(crate) fn seek(fd: c_int, offset: i64, whence: c_int) -> i64 {
    let _guard = FILES_LOCK.lock();
    let Some(crate::fd::Kind::File { size, offset: current, .. }) = crate::fd::get(fd) else {
        return -1;
    };
    let base = match whence {
        0 => 0,                   // SEEK_SET
        1 => current as i64,      // SEEK_CUR
        2 => size as i64,         // SEEK_END
        _ => return -1,
    };
    let Some(target) = base.checked_add(offset).filter(|t| *t >= 0) else {
        return -1;
    };
    // Seeking past the end is legal in C and simply reads nothing later; clamping
    // it here would silently turn a wrong offset into a plausible one.
    crate::fd::set_offset(fd, target as u64);
    target
}

/// The size a `stat`/`fstat` would report, or `None`.
pub(crate) fn size_of_path(path: &[u8]) -> Option<u64> {
    let _guard = FILES_LOCK.lock();
    let len = put_path(path)?;
    let reply = request(TAG_STAT, [len as u64, 0, 0], true)?;
    (reply.tag != TAG_ERROR).then_some(reply.words[0])
}

/// The size behind an open descriptor.
pub(crate) fn size_of_fd(fd: c_int) -> Option<u64> {
    match crate::fd::get(fd)? {
        crate::fd::Kind::File { size, .. } => Some(size),
        _ => None,
    }
}

/// Tell the server a handle is finished with. The descriptor itself is released by
/// [`crate::fd::release`], which is what knows about the other kinds.
pub(crate) fn close_handle(handle: u64) -> c_int {
    let _guard = FILES_LOCK.lock();
    match request(TAG_CLOSE, [handle, 0, 0], false) {
        Some(reply) if reply.tag != TAG_ERROR => 0,
        // The descriptor is released either way: a server that refuses to close a
        // handle must not also cost this process a descriptor forever.
        _ => -1,
    }
}

/// Whether a path exists, for `access`. Distinguishes "no such file" from "the
/// server did not answer" the only way a client can: by the error it got back.
pub(crate) fn exists(path: &[u8]) -> bool {
    let _guard = FILES_LOCK.lock();
    let Some(len) = put_path(path) else {
        return false;
    };
    match request(TAG_STAT, [len as u64, 0, 0], true) {
        Some(reply) => reply.tag != TAG_ERROR || reply.words[0] != ERR_NO_FILE,
        None => false,
    }
}

/// The C entry points.
#[cfg(not(test))]
pub mod exports {
    use super::{c_char, c_int};

    /// # Safety
    /// C ABI: `path` is a NUL-terminated string. Flags are accepted and ignored —
    /// there is nothing to write to.
    #[no_mangle]
    pub unsafe extern "C" fn open(path: *const c_char, _flags: c_int, _mode: c_int) -> c_int {
        // SAFETY: forwarded from the caller.
        super::open(unsafe { crate::string::as_bytes(path) })
    }

    /// # Safety
    /// C ABI: `buf` is valid for `count` bytes.
    ///
    /// One `read` for every kind of descriptor: a file goes to the file server, an
    /// eventfd or a pipe to [`crate::fd`]. Programs do not know which they were
    /// handed — that is the point of a descriptor — so the dispatch belongs here
    /// rather than in the caller.
    #[no_mangle]
    pub unsafe extern "C" fn read(fd: c_int, buf: *mut core::ffi::c_void, count: usize) -> isize {
        if fd <= 2 {
            // stdin is not a thing yet; a program that reads it gets end of file
            // rather than a hang.
            return 0;
        }
        // SAFETY: forwarded from the caller.
        let dst = unsafe { core::slice::from_raw_parts_mut(buf.cast::<u8>(), count) };
        match crate::fd::get(fd) {
            Some(crate::fd::Kind::File { .. }) => super::read(fd, dst),
            Some(kind) => crate::fd::read(kind, dst),
            None => -1,
        }
    }

    /// # Safety
    /// C ABI: `buf` is valid for `count` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn write(fd: c_int, buf: *const core::ffi::c_void, count: usize) -> isize {
        // SAFETY: forwarded from the caller.
        let src = unsafe { core::slice::from_raw_parts(buf.cast::<u8>(), count) };
        if fd == 1 || fd == 2 {
            crate::stdio::write_bytes(src);
            return count as isize;
        }
        match crate::fd::get(fd) {
            // The file server is read-only, and saying so beats accepting bytes
            // that go nowhere.
            Some(crate::fd::Kind::File { .. }) | None => -1,
            Some(kind) => crate::fd::write(kind, src),
        }
    }

    #[no_mangle]
    pub extern "C" fn close(fd: c_int) -> c_int {
        match crate::fd::release(fd) {
            Some(handle) => super::close_handle(handle),
            None => 0,
        }
    }

    #[no_mangle]
    pub extern "C" fn lseek(fd: c_int, offset: i64, whence: c_int) -> i64 {
        super::seek(fd, offset, whence)
    }

    /// # Safety
    /// C ABI: `path` is NUL-terminated.
    #[no_mangle]
    pub unsafe extern "C" fn access(path: *const c_char, _mode: c_int) -> c_int {
        // SAFETY: forwarded from the caller.
        if super::exists(unsafe { crate::string::as_bytes(path) }) {
            0
        } else {
            -1
        }
    }

    /// The subset of `struct stat` this system can fill in truthfully.
    ///
    /// It is *not* layout-compatible with glibc's — nothing here parses a C header,
    /// and a program that memcpy's this into one will get nonsense. When Qt is
    /// built against this library it will be built against this crate's own header,
    /// which is the only way the two can agree.
    #[repr(C)]
    pub struct Stat {
        pub size: u64,
        pub mode: u32,
        pub is_dir: u32,
    }

    /// # Safety
    /// C ABI: `out` is valid for one `Stat`.
    #[no_mangle]
    pub unsafe extern "C" fn fstat(fd: c_int, out: *mut Stat) -> c_int {
        let Some(size) = super::size_of_fd(fd) else {
            return -1;
        };
        // SAFETY: forwarded from the caller.
        unsafe {
            (*out).size = size;
            (*out).mode = 0o100644;
            (*out).is_dir = 0;
        }
        0
    }

    /// # Safety
    /// C ABI: `path` NUL-terminated, `out` valid for one `Stat`.
    #[no_mangle]
    pub unsafe extern "C" fn stat(path: *const c_char, out: *mut Stat) -> c_int {
        // SAFETY: forwarded from the caller.
        let Some(size) = super::size_of_path(unsafe { crate::string::as_bytes(path) }) else {
            return -1;
        };
        // SAFETY: forwarded from the caller.
        unsafe {
            (*out).size = size;
            (*out).mode = 0o100644;
            (*out).is_dir = 0;
        }
        0
    }
}
