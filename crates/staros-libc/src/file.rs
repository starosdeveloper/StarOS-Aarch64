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
    ERR_NO_FILE, MAX_PATH, TAG_BYE, TAG_CLOSE, TAG_ERROR, TAG_LIST, TAG_OPEN, TAG_READ, TAG_STAT,
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
/// Strip what an absolute path means on a system with one filesystem.
///
/// The file server serves the initramfs, and CPIO stores its members without a
/// leading slash: the archive's own name for the scene is `qml/Main.qml`. A program
/// that opens `/qml/Main.qml` means the same file — there is nowhere else it could
/// mean — so the two spellings have to reach the same member.
///
/// This is not tidiness, it is what makes the filesystem usable from a toolkit. Qt
/// turns every path into a URL and every relative URL into an absolute one against
/// the working directory, which is `/` here: `QUrl::fromLocalFile("qml/Main.qml")`
/// becomes `file:///qml/Main.qml` and the open then failed with "No such file or
/// directory" for a file the same program had just read by its relative name.
///
/// `./` goes too, and repeated slashes with it. What is deliberately *not* here is
/// `..`: this is a name transformation and not a path resolver, and a system with
/// one flat archive has no directory to go up from. A path containing `..` is passed
/// through and the server refuses it, which is the truthful outcome.
fn normalise(path: &[u8]) -> &[u8] {
    let mut at = 0;
    loop {
        if path[at..].starts_with(b"/") {
            at += 1;
        } else if path[at..].starts_with(b"./") {
            at += 2;
        } else {
            break;
        }
    }
    &path[at..]
}

fn put_path(path: &[u8]) -> Option<usize> {
    let path = normalise(path);
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

/// The archive's `index`-th member: its name (into `name`), size and mode.
///
/// The whole of `readdir` on this system. There is no directory object on the
/// server — the archive is flat and read-only — so a listing is a walk over indices
/// and the client does the filtering.
pub(crate) fn list(index: u64, name: &mut [u8]) -> Option<(usize, u64, u32)> {
    let _guard = FILES_LOCK.lock();
    let reply = request(TAG_LIST, [index, 0, 0], true)?;
    if reply.tag == TAG_ERROR {
        return None;
    }
    let len = (reply.words[0] as usize).min(name.len());
    // SAFETY: read under the file lock, from the page this process mapped; `len` is
    // bounded by both the reply and the caller's buffer.
    let files = unsafe { &*core::ptr::addr_of!(FILES) };
    // SAFETY: as above.
    unsafe { core::ptr::copy_nonoverlapping(files.buffer, name.as_mut_ptr(), len) };
    Some((len, reply.words[1], reply.words[2] as u32))
}

/// The size and mode a `stat` would report, or `None`.
pub(crate) fn stat_of_path(path: &[u8]) -> Option<(u64, u32)> {
    let _guard = FILES_LOCK.lock();
    let len = put_path(path)?;
    let reply = request(TAG_STAT, [len as u64, 0, 0], true)?;
    (reply.tag != TAG_ERROR).then_some((reply.words[0], reply.words[1] as u32))
}

/// Whether any archive member lies under this path — which is what "a directory
/// exists" means in a flat archive.
///
/// The archive stores paths, not directories: `fonts/DejaVuSans.ttf` is a member
/// and `fonts` is not. So a directory here is a prefix that something uses, and the
/// root always exists even when the archive is empty.
pub(crate) fn directory_exists(path: &[u8]) -> bool {
    let path = trim_slashes(path);
    if path.is_empty() || path == b"." {
        return true;
    }
    let mut name = [0u8; MAX_PATH];
    let mut index = 0;
    while let Some((len, _, _)) = list(index, &mut name) {
        let entry = trim_slashes(&name[..len]);
        // A member *under* the prefix, not the prefix itself: `fonts` is a
        // directory because `fonts/x.ttf` exists, and `fonts.txt` does not make it
        // one — which is why the separator is part of the test.
        if entry.len() > path.len()
            && entry.starts_with(path)
            && entry[path.len()] == b'/'
        {
            return true;
        }
        index += 1;
    }
    false
}

/// Drop leading `./` and any leading or trailing `/`, so that `/fonts/`, `fonts`
/// and `./fonts` are one path.
pub(crate) fn trim_slashes(path: &[u8]) -> &[u8] {
    let mut p = path;
    while let Some(rest) = p.strip_prefix(b"./") {
        p = rest;
    }
    while let Some(rest) = p.strip_prefix(b"/") {
        p = rest;
    }
    while let Some(rest) = p.strip_suffix(b"/") {
        p = rest;
    }
    p
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
    use super::{c_char, c_int, MAX_PATH};

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
            Some(kind) => crate::fd::read(kind, dst, crate::fd::is_nonblock(fd)),
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
            Some(kind) => crate::fd::write(kind, src, crate::fd::is_nonblock(fd)),
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

    /// `struct stat`, in **glibc's AArch64 layout**, field for field.
    ///
    /// The previous version of this structure was a convenient three fields, with a
    /// comment saying a program that memcpy'd it into glibc's would get nonsense.
    /// That was true and it was not good enough: libstdc++'s `std::filesystem` and
    /// Qt's `QFileInfo` were compiled against glibc's headers years before this
    /// library existed, and they will read `st_mode` at offset 16 and `st_size` at
    /// offset 48 whatever a header here says. So this is that layout, padding
    /// included, with static assertions below to keep it that way.
    ///
    /// Most of it is honestly zero: there are no inodes, no owners and no
    /// timestamps in a CPIO archive served read-only. Zero is a *value* a caller can
    /// reason about, and `st_mode` and `st_size` — the two fields anything actually
    /// branches on — are real.
    #[repr(C)]
    #[derive(Default)]
    pub struct Stat {
        pub st_dev: u64,
        pub st_ino: u64,
        pub st_mode: u32,
        pub st_nlink: u32,
        pub st_uid: u32,
        pub st_gid: u32,
        pub st_rdev: u64,
        pub __pad1: u64,
        pub st_size: i64,
        pub st_blksize: i32,
        pub __pad2: i32,
        pub st_blocks: i64,
        pub st_atime: i64,
        pub st_atime_nsec: i64,
        pub st_mtime: i64,
        pub st_mtime_nsec: i64,
        pub st_ctime: i64,
        pub st_ctime_nsec: i64,
        pub __unused: [u32; 2],
    }

    const _: () = {
        assert!(core::mem::size_of::<Stat>() == 128);
        assert!(core::mem::offset_of!(Stat, st_mode) == 16);
        assert!(core::mem::offset_of!(Stat, st_size) == 48);
        assert!(core::mem::offset_of!(Stat, st_blocks) == 64);
        assert!(core::mem::offset_of!(Stat, st_mtime) == 88);
    };

    /// `S_IFREG`/`S_IFDIR`, which is what a caller tests `st_mode` against.
    const S_IFREG: u32 = 0o100_000;
    const S_IFDIR: u32 = 0o040_000;

    /// Fill a `Stat` from a size and the archive's mode bits.
    ///
    /// # Safety
    /// `out` is valid for one `Stat`.
    unsafe fn fill(out: *mut Stat, size: u64, mode: u32) {
        if out.is_null() {
            return;
        }
        // A CPIO archive carries real mode bits, so the file type comes from the
        // archive rather than from an assumption. Only when it carries none — an
        // entry that predates the field, or the empty-archive case — is a regular
        // file assumed, and 0644 is then the mode of everything in the initramfs.
        let mode = if mode & 0o170_000 == 0 { S_IFREG | 0o644 } else { mode };
        // SAFETY: the caller's contract.
        unsafe {
            out.write(Stat {
                st_mode: mode,
                st_nlink: 1,
                st_size: size as i64,
                st_blksize: 4096,
                st_blocks: size.div_ceil(512) as i64,
                ..Stat::default()
            });
        }
    }

    /// # Safety
    /// C ABI: `out` is valid for one `Stat`.
    #[no_mangle]
    pub unsafe extern "C" fn fstat(fd: c_int, out: *mut Stat) -> c_int {
        let Some(size) = super::size_of_fd(fd) else {
            // A descriptor that is not a file is still a descriptor: a pipe or an
            // eventfd stats as a FIFO with no size, which is what a program that
            // calls fstat on one is asking about.
            if crate::fd::get(fd).is_some() || (0..=2).contains(&fd) {
                // SAFETY: forwarded from the caller.
                unsafe { fill(out, 0, 0o010_000 | 0o600) };
                return 0;
            }
            return crate::fail(9, -1); // EBADF
        };
        // SAFETY: forwarded from the caller.
        unsafe { fill(out, size, 0) };
        0
    }

    /// # Safety
    /// C ABI: `path` NUL-terminated, `out` valid for one `Stat`.
    #[no_mangle]
    pub unsafe extern "C" fn stat(path: *const c_char, out: *mut Stat) -> c_int {
        // SAFETY: forwarded from the caller.
        let bytes = unsafe { crate::string::as_bytes(path) };
        if let Some((size, mode)) = super::stat_of_path(bytes) {
            // SAFETY: forwarded from the caller.
            unsafe { fill(out, size, mode) };
            return 0;
        }
        // Not a file — but it may still be a directory prefix that the flat archive
        // implies. `stat("/fonts")` has to succeed for a program that checks a
        // directory before listing it.
        if super::directory_exists(bytes) {
            // SAFETY: forwarded from the caller.
            unsafe { fill(out, 0, S_IFDIR | 0o755) };
            return 0;
        }
        crate::fail(2, -1) // ENOENT
    }

    /// There are no symbolic links in a CPIO archive this system unpacks, so
    /// `lstat` is `stat`. Saying so here is better than an alias in a header,
    /// because it is the behaviour rather than the spelling that matters.
    ///
    /// # Safety
    /// As [`stat`].
    #[no_mangle]
    pub unsafe extern "C" fn lstat(path: *const c_char, out: *mut Stat) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { stat(path, out) }
    }

    /// The `*64` names, which glibc's headers redirect to on a 32-bit target and
    /// which Qt's objects therefore reference. Same function: `off_t` is already
    /// 64 bits here.
    ///
    /// # Safety
    /// As [`stat`].
    #[no_mangle]
    pub unsafe extern "C" fn stat64(path: *const c_char, out: *mut Stat) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { stat(path, out) }
    }

    /// # Safety
    /// As [`stat`].
    #[no_mangle]
    pub unsafe extern "C" fn lstat64(path: *const c_char, out: *mut Stat) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { stat(path, out) }
    }

    /// # Safety
    /// As [`fstat`].
    #[no_mangle]
    pub unsafe extern "C" fn fstat64(fd: c_int, out: *mut Stat) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { fstat(fd, out) }
    }

    /// # Safety
    /// C ABI: `path` NUL-terminated, `out` valid for one `Stat`.
    #[no_mangle]
    pub unsafe extern "C" fn fstatat(
        _dirfd: c_int,
        path: *const c_char,
        out: *mut Stat,
        _flags: c_int,
    ) -> c_int {
        // There is one directory — the archive — so every `dirfd` names it.
        // SAFETY: forwarded from the caller.
        unsafe { stat(path, out) }
    }

    /// # Safety
    /// As [`fstatat`].
    #[no_mangle]
    pub unsafe extern "C" fn fstatat64(
        dirfd: c_int,
        path: *const c_char,
        out: *mut Stat,
        flags: c_int,
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { fstatat(dirfd, path, out, flags) }
    }

    /// `struct statx`, Linux's replacement for `stat`, in the kernel's layout.
    ///
    /// Qt reaches this one on a modern glibc — `stat` is a wrapper around it there —
    /// so a program that links against it gets the timestamps and the size, not a
    /// refusal. The nested timestamp is a separate structure in the ABI and is
    /// spelled out here rather than flattened, because the offsets are what a caller
    /// compiled against `<linux/stat.h>` expects.
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct StatxTimestamp {
        pub tv_sec: i64,
        pub tv_nsec: u32,
        pub __reserved: i32,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct Statx {
        pub stx_mask: u32,
        pub stx_blksize: u32,
        pub stx_attributes: u64,
        pub stx_nlink: u32,
        pub stx_uid: u32,
        pub stx_gid: u32,
        pub stx_mode: u16,
        pub __spare0: [u16; 1],
        pub stx_ino: u64,
        pub stx_size: u64,
        pub stx_blocks: u64,
        pub stx_attributes_mask: u64,
        pub stx_atime: StatxTimestamp,
        pub stx_btime: StatxTimestamp,
        pub stx_ctime: StatxTimestamp,
        pub stx_mtime: StatxTimestamp,
        pub stx_rdev_major: u32,
        pub stx_rdev_minor: u32,
        pub stx_dev_major: u32,
        pub stx_dev_minor: u32,
        pub stx_mnt_id: u64,
        pub stx_dio_mem_align: u32,
        pub stx_dio_offset_align: u32,
        pub __spare3: [u64; 12],
    }

    const _: () = {
        assert!(core::mem::size_of::<Statx>() == 256);
        assert!(core::mem::offset_of!(Statx, stx_mode) == 28);
        assert!(core::mem::offset_of!(Statx, stx_size) == 40);
        // The four timestamps are in the kernel's order — atime, btime, ctime,
        // mtime — which is not the order `struct stat` uses.
        assert!(core::mem::offset_of!(Statx, stx_atime) == 64);
        assert!(core::mem::offset_of!(Statx, stx_ctime) == 96);
        assert!(core::mem::offset_of!(Statx, stx_mtime) == 112);
    };

    /// The `stx_mask` bits this filesystem can actually answer. Reporting only these
    /// is the point of the mask: a caller that asked for `STATX_BTIME` is told the
    /// birth time is not among what came back, rather than handed a zero that looks
    /// like 1970.
    const STATX_TYPE: u32 = 0x0001;
    const STATX_MODE: u32 = 0x0002;
    const STATX_NLINK: u32 = 0x0004;
    const STATX_SIZE: u32 = 0x0200;
    const STATX_BLOCKS: u32 = 0x0400;
    const AT_EMPTY_PATH: c_int = 0x1000;

    /// # Safety
    /// C ABI: `path` NUL-terminated, `out` valid for one `Statx`.
    #[no_mangle]
    pub unsafe extern "C" fn statx(
        dirfd: c_int,
        path: *const c_char,
        flags: c_int,
        _mask: u32,
        out: *mut Statx,
    ) -> c_int {
        if out.is_null() {
            return crate::fail(22, -1); // EFAULT is not distinguishable here
        }
        // `statx` doubles as `fstat` when the path is empty and the caller says so,
        // which is how glibc implements `fstat` on a modern kernel.
        // SAFETY: forwarded from the caller.
        let bytes = unsafe { crate::string::as_bytes(path) };
        let mut st = Stat::default();
        let rc = if bytes.is_empty() && flags & AT_EMPTY_PATH != 0 {
            // SAFETY: `st` is one initialised `Stat` on this stack.
            unsafe { fstat(dirfd, &raw mut st) }
        } else {
            // SAFETY: as above.
            unsafe { stat(path, &raw mut st) }
        };
        if rc != 0 {
            return rc;
        }
        // SAFETY: the caller's contract, checked non-null above.
        unsafe {
            out.write(Statx {
                stx_mask: STATX_TYPE | STATX_MODE | STATX_NLINK | STATX_SIZE | STATX_BLOCKS,
                stx_blksize: st.st_blksize as u32,
                stx_nlink: st.st_nlink,
                stx_mode: st.st_mode as u16,
                stx_size: st.st_size as u64,
                stx_blocks: st.st_blocks as u64,
                ..Statx::default()
            });
        }
        0
    }

    /// `struct statfs`, in the AArch64 layout. What this reports is the truth about
    /// an initramfs: a filesystem with no free space, because nothing can be written
    /// to it, and a file count it knows exactly because the archive is finite.
    #[repr(C)]
    #[derive(Default)]
    pub struct Statfs {
        pub f_type: i64,
        pub f_bsize: i64,
        pub f_blocks: u64,
        pub f_bfree: u64,
        pub f_bavail: u64,
        pub f_files: u64,
        pub f_ffree: u64,
        pub f_fsid: [i32; 2],
        pub f_namelen: i64,
        pub f_frsize: i64,
        pub f_flags: i64,
        pub f_spare: [i64; 4],
    }

    const _: () = {
        assert!(core::mem::size_of::<Statfs>() == 120);
        assert!(core::mem::offset_of!(Statfs, f_files) == 40);
        assert!(core::mem::offset_of!(Statfs, f_namelen) == 64);
    };

    /// `RAMFS_MAGIC`, which is what this is: an archive unpacked into memory.
    const RAMFS_MAGIC: i64 = 0x8584_58f6;
    /// `ST_RDONLY`, the flag that says why every write fails.
    const ST_RDONLY: i64 = 1;

    /// Count the archive's members and their bytes, by walking it.
    fn archive_totals() -> (u64, u64) {
        let mut name = [0u8; MAX_PATH];
        let (mut files, mut bytes) = (0u64, 0u64);
        let mut index = 0;
        while let Some((_, size, _)) = super::list(index, &mut name) {
            files += 1;
            bytes += size;
            index += 1;
        }
        (files, bytes)
    }

    /// # Safety
    /// C ABI: `out` is valid for one `Statfs`.
    #[no_mangle]
    pub unsafe extern "C" fn statfs(_path: *const c_char, out: *mut Statfs) -> c_int {
        if out.is_null() {
            return crate::fail(22, -1); // EINVAL
        }
        let (files, bytes) = archive_totals();
        let blocks = bytes.div_ceil(4096);
        // SAFETY: the caller's contract, checked non-null above.
        unsafe {
            out.write(Statfs {
                f_type: RAMFS_MAGIC,
                f_bsize: 4096,
                f_frsize: 4096,
                f_blocks: blocks,
                // No free blocks and no free inodes: a program deciding whether it
                // can write a cache file here should decide no, and this is the
                // number it looks at.
                f_bfree: 0,
                f_bavail: 0,
                f_files: files,
                f_ffree: 0,
                f_namelen: MAX_PATH as i64,
                f_flags: ST_RDONLY,
                ..Statfs::default()
            });
        }
        0
    }

    /// # Safety
    /// As [`statfs`].
    #[no_mangle]
    pub unsafe extern "C" fn statfs64(path: *const c_char, out: *mut Statfs) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { statfs(path, out) }
    }

    /// # Safety
    /// As [`statfs`], with a descriptor naming the same one filesystem.
    #[no_mangle]
    pub unsafe extern "C" fn fstatfs(_fd: c_int, out: *mut Statfs) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { statfs(core::ptr::null(), out) }
    }

    /// # Safety
    /// As [`fstatfs`].
    #[no_mangle]
    pub unsafe extern "C" fn fstatfs64(fd: c_int, out: *mut Statfs) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { fstatfs(fd, out) }
    }

    /// `sendfile`: copy from a file to a descriptor without the caller's buffer.
    ///
    /// This one *works* rather than refusing, because the destination that matters
    /// is the console: the source is a file in the archive and the sink is a pipe or
    /// standard output, both of which this library can write. The copy is real —
    /// through a bounded stack buffer, one page at a time — and it stops at whatever
    /// the write accepted, which is what a caller resumes from.
    ///
    /// # Safety
    /// C ABI: `offset` is null or valid for one `i64`.
    #[no_mangle]
    pub unsafe extern "C" fn sendfile(
        out_fd: c_int,
        in_fd: c_int,
        offset: *mut i64,
        count: usize,
    ) -> isize {
        let mut buf = [0u8; 4096];
        let mut done = 0usize;
        // An explicit offset does not disturb the descriptor's own position, which is
        // the difference between `sendfile(…, &off, n)` and `sendfile(…, 0, n)` and
        // the reason a caller passes one.
        let saved = if offset.is_null() {
            None
        } else {
            let here = super::seek(in_fd, 0, 1); // SEEK_CUR
            if here < 0 {
                return crate::fail(9, -1); // EBADF
            }
            // SAFETY: the caller's contract.
            let start = unsafe { *offset };
            if super::seek(in_fd, start, 0) < 0 {
                return crate::fail(22, -1); // EINVAL
            }
            Some(here)
        };
        while done < count {
            let want = (count - done).min(buf.len());
            let got = super::read(in_fd, &mut buf[..want]);
            if got < 0 {
                return crate::fail(5, -1); // EIO
            }
            if got == 0 {
                break; // end of file: fewer bytes than asked for is not an error
            }
            let wrote = write_bytes(out_fd, &buf[..got as usize]);
            if wrote < 0 {
                // Nothing copied at all is the caller's error to see; a partial copy
                // is a short count, which is what the interface is for.
                return if done == 0 { -1 } else { done as isize };
            }
            done += wrote as usize;
            if wrote < got {
                break; // the sink took less than the file gave
            }
        }
        if let Some(here) = saved {
            let ended = super::seek(in_fd, 0, 1);
            super::seek(in_fd, here, 0);
            // SAFETY: the caller's contract; non-null in this branch.
            unsafe { *offset = ended };
        }
        done as isize
    }

    /// The write half of [`sendfile`], as an `isize` rather than through the
    /// variadic C entry point.
    fn write_bytes(fd: c_int, bytes: &[u8]) -> isize {
        // SAFETY: `bytes` is a live slice for the length given.
        unsafe { write(fd, bytes.as_ptr().cast(), bytes.len()) }
    }

    /// `sendfile64`: the same call; `off_t` is already 64 bits here.
    ///
    /// # Safety
    /// As [`sendfile`].
    #[no_mangle]
    pub unsafe extern "C" fn sendfile64(
        out_fd: c_int,
        in_fd: c_int,
        offset: *mut i64,
        count: usize,
    ) -> isize {
        // SAFETY: forwarded from the caller.
        unsafe { sendfile(out_fd, in_fd, offset, count) }
    }

    /// `copy_file_range`: both ends must be files, and the destination end cannot
    /// exist here — every file is in a read-only archive. `EXDEV` is the documented
    /// answer for a copy this call cannot make, and it is the one that makes a caller
    /// fall back to reading and writing itself rather than give up.
    ///
    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn copy_file_range(
        _in_fd: c_int,
        _in_off: *mut i64,
        _out_fd: c_int,
        _out_off: *mut i64,
        _len: usize,
        _flags: u32,
    ) -> isize {
        crate::fail(18, -1) // EXDEV
    }

    /// `open64`, `openat`: the same open. Flags asking to write are refused here
    /// rather than at the first `write`, for the reason `fopen` refuses them.
    ///
    /// # Safety
    /// As [`open`].
    #[no_mangle]
    pub unsafe extern "C" fn open64(path: *const c_char, flags: c_int, mode: c_int) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { open(path, flags, mode) }
    }

    /// # Safety
    /// As [`open`].
    #[no_mangle]
    pub unsafe extern "C" fn openat(
        _dirfd: c_int,
        path: *const c_char,
        flags: c_int,
        mode: c_int,
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { open(path, flags, mode) }
    }

    #[no_mangle]
    pub extern "C" fn lseek64(fd: c_int, offset: i64, whence: c_int) -> i64 {
        super::seek(fd, offset, whence)
    }

    /// `isatty`: no. There is a console, but it is not a terminal — no line
    /// discipline, no window size, no input. A program told "yes" starts asking
    /// about the size of a window that does not exist.
    #[no_mangle]
    pub extern "C" fn isatty(_fd: c_int) -> c_int {
        crate::fail(25, 0) // ENOTTY, which is what C says to set
    }

    /// `fcntl`, for the requests that have an answer here.
    ///
    /// `F_SETFL` used to return 0 and change nothing. That is the worst of the three
    /// possible answers: a caller that sets `O_NONBLOCK` and is told it succeeded
    /// goes on to write a drain loop — `while (read(fd, buf, n) > 0) {}` — which
    /// this libc then blocked in forever, several thousand frames from the `fcntl`
    /// that caused it. Qt's event dispatcher is exactly that caller, and this is
    /// exactly where its event loop stopped. The flag is now real; see
    /// [`crate::fd::set_nonblock`].
    ///
    /// # Safety
    /// C ABI, variadic in the third argument.
    #[no_mangle]
    pub unsafe extern "C" fn fcntl(fd: c_int, cmd: c_int, mut args: ...) -> c_int {
        const F_GETFD: c_int = 1;
        const F_SETFD: c_int = 2;
        const F_GETFL: c_int = 3;
        const F_SETFL: c_int = 4;
        match cmd {
            // No close-on-exec flag, because there is no exec.
            F_GETFD => 0,
            F_SETFD => 0,
            // O_RDONLY — everything here is — plus whatever blocking mode this
            // descriptor is actually in.
            F_GETFL => {
                if crate::fd::is_nonblock(fd) {
                    crate::fd::O_NONBLOCK
                } else {
                    0
                }
            }
            // Only `O_NONBLOCK` is answerable. The access mode cannot be changed
            // after the fact and `O_APPEND` has no meaning on a read-only
            // filesystem, so both are ignored here exactly as POSIX allows.
            F_SETFL => {
                // SAFETY: C ABI — `F_SETFL` is defined to take one `int`.
                let flags = unsafe { args.next_arg::<c_int>() };
                if crate::fd::set_nonblock(fd, flags & crate::fd::O_NONBLOCK != 0) {
                    0
                } else {
                    crate::fail(9, -1) // EBADF
                }
            }
            _ => {
                let _ = fd;
                crate::fail(22, -1) // EINVAL
            }
        }
    }

    /// `flock`: refused with `ENOLCK`, for the same reason `fcntl`'s locking
    /// commands are.
    ///
    /// A lock is a promise about what some *other* process will be prevented from
    /// doing, and nothing here can make that promise: the filesystem is one
    /// read-only archive, there is no lock table in the file server, and a second
    /// process asking for the same lock would be told yes as readily as the first.
    /// Returning 0 is the tempting answer — every caller's happy path — and it is
    /// exactly the wrong one, because a caller that believes it holds an exclusive
    /// lock proceeds to do the thing the lock was protecting.
    ///
    /// `ENOLCK` rather than `ENOSYS`: POSIX defines it as "no locks available",
    /// which is precisely the situation, and `flock`'s callers are written to expect
    /// it. Qt's `QLockFile` treats it as a failure to acquire and reports that
    /// upward, which is true.
    #[no_mangle]
    pub extern "C" fn flock(_fd: c_int, _operation: c_int) -> c_int {
        crate::fail(37, -1) // ENOLCK
    }

    /// `ioctl`: there are no devices behind these descriptors, so every request is
    /// refused. A libc that returned 0 would tell a program its terminal request
    /// succeeded and leave it using an uninitialised `struct winsize`.
    ///
    /// # Safety
    /// C ABI, variadic.
    #[no_mangle]
    pub unsafe extern "C" fn ioctl(_fd: c_int, _request: u64, _args: ...) -> c_int {
        crate::fail(25, -1) // ENOTTY
    }

    /// `tmpnam`: a name no file has. There is no writable filesystem here, so
    /// there is no name a caller could then create, and C allows returning null
    /// when one cannot be produced.
    ///
    /// Null rather than a plausible `/tmp/...`: a name that looks usable sends the
    /// caller to `fopen`, which refuses with `EROFS` several frames away from the
    /// decision that caused it.
    ///
    /// # Safety
    /// C ABI: `s` is ignored, and nothing is written through it.
    #[no_mangle]
    pub unsafe extern "C" fn tmpnam(_s: *mut c_char) -> *mut c_char {
        core::ptr::null_mut()
    }

    /// Everything that changes the filesystem, refused with `EROFS` — one place, so
    /// that the list of what this system cannot do is readable rather than scattered.
    ///
    /// `EROFS` and not `ENOSYS`: the call is implemented, the filesystem is
    /// read-only. A program that sees `ENOSYS` may conclude the libc is incomplete
    /// and try a fallback path; `EROFS` tells it the truth, which is that no path
    /// will work.
    macro_rules! read_only {
        ($($name:ident($($arg:ident: $ty:ty),*)),* $(,)?) => {$(
            /// # Safety
            /// C ABI.
            #[no_mangle]
            pub unsafe extern "C" fn $name($($arg: $ty),*) -> c_int {
                $(let _ = $arg;)*
                crate::fail(30, -1) // EROFS
            }
        )*};
    }

    read_only! {
        // `remove` is C's name for "unlink a file or rmdir a directory", and it
        // belongs in this list for the same reason both of those do.
        remove(path: *const c_char),
        mkdir(path: *const c_char, mode: u32),
        mkdirat(dirfd: c_int, path: *const c_char, mode: u32),
        rmdir(path: *const c_char),
        unlink(path: *const c_char),
        unlinkat(dirfd: c_int, path: *const c_char, flags: c_int),
        rename(from: *const c_char, to: *const c_char),
        renameat(fromfd: c_int, from: *const c_char, tofd: c_int, to: *const c_char),
        renameat2(fromfd: c_int, from: *const c_char, tofd: c_int, to: *const c_char, flags: u32),
        link(from: *const c_char, to: *const c_char),
        linkat(fromfd: c_int, from: *const c_char, tofd: c_int, to: *const c_char, flags: c_int),
        symlink(target: *const c_char, path: *const c_char),
        chmod(path: *const c_char, mode: u32),
        fchmod(fd: c_int, mode: u32),
        truncate(path: *const c_char, length: i64),
        truncate64(path: *const c_char, length: i64),
        ftruncate(fd: c_int, length: i64),
        ftruncate64(fd: c_int, length: i64),
        futimens(fd: c_int, times: *const core::ffi::c_void),
        utimensat(dirfd: c_int, path: *const c_char, times: *const core::ffi::c_void, flags: c_int),
        chdir(path: *const c_char),
        fchdir(fd: c_int),
        // `flock` was here and has moved out, because `EROFS` was the wrong reason.
        // A *shared* lock on a file opened for reading is a legal thing to want on a
        // read-only filesystem, so "read-only filesystem" does not explain the
        // refusal; see [`flock`] for the one that does.
        shm_open(name: *const c_char, flags: c_int, mode: u32),
        shm_unlink(name: *const c_char),
    }

    /// `readlink`: nothing here is a symbolic link, so every path fails with
    /// `EINVAL` — which is precisely what POSIX says to return for a path that is
    /// not one, and lets a caller tell it apart from a missing file.
    ///
    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn readlink(
        _path: *const c_char,
        _buf: *mut c_char,
        _size: usize,
    ) -> isize {
        crate::fail(22, -1) // EINVAL
    }

    /// `fsync`/`fdatasync`: nothing is buffered on the way to storage, because
    /// nothing goes to storage. Success is the truthful answer — everything that
    /// was written is as durable as it is ever going to be.
    #[no_mangle]
    pub extern "C" fn fsync(_fd: c_int) -> c_int {
        0
    }

    #[no_mangle]
    pub extern "C" fn fdatasync(_fd: c_int) -> c_int {
        0
    }

    /// The working directory, which is the archive's root and cannot be changed —
    /// see `chdir` above.
    ///
    /// # Safety
    /// C ABI: `buf` is valid for `size` bytes, or null.
    #[no_mangle]
    pub unsafe extern "C" fn getcwd(buf: *mut c_char, size: usize) -> *mut c_char {
        const CWD: &[u8] = b"/\0";
        if buf.is_null() {
            // The GNU extension: allocate. Programs use it, and returning null here
            // would send them down an error path over a working directory that is
            // perfectly well known.
            let p = crate::heap_alloc(CWD.len(), 1).cast::<c_char>();
            if p.is_null() {
                return crate::fail(12, core::ptr::null_mut()); // ENOMEM
            }
            // SAFETY: `p` is a fresh allocation of exactly this length.
            unsafe { core::ptr::copy_nonoverlapping(CWD.as_ptr().cast::<c_char>(), p, CWD.len()) };
            return p;
        }
        if size < CWD.len() {
            return crate::fail(34, core::ptr::null_mut()); // ERANGE
        }
        // SAFETY: checked against `size` above.
        unsafe { core::ptr::copy_nonoverlapping(CWD.as_ptr().cast::<c_char>(), buf, CWD.len()) };
        buf
    }

    /// `realpath`: with no symbolic links, no `..` in the archive and one working
    /// directory, the resolved path is the path — but only if it exists, which is
    /// the part callers rely on.
    ///
    /// # Safety
    /// C ABI: `resolved` is null or valid for `PATH_MAX` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn realpath(path: *const c_char, resolved: *mut c_char) -> *mut c_char {
        // SAFETY: forwarded from the caller.
        let bytes = unsafe { crate::string::as_bytes(path) };
        if !super::exists(bytes) && !super::directory_exists(bytes) {
            return crate::fail(2, core::ptr::null_mut()); // ENOENT
        }
        let out = if resolved.is_null() {
            let p = crate::heap_alloc(bytes.len() + 1, 1).cast::<c_char>();
            if p.is_null() {
                return crate::fail(12, core::ptr::null_mut());
            }
            p
        } else {
            resolved
        };
        // SAFETY: `out` has room for the path and its NUL — either freshly
        // allocated for exactly that, or the caller's PATH_MAX buffer.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), out, bytes.len());
            *out.add(bytes.len()) = 0;
        }
        out
    }

    /// The fortified `realpath`, which is in the contract because Qt is built with
    /// `_FORTIFY_SOURCE`: same function, plus the buffer size the compiler knows.
    ///
    /// # Safety
    /// As [`realpath`], with `size` describing `resolved`.
    #[no_mangle]
    pub unsafe extern "C" fn __realpath_chk(
        path: *const c_char,
        resolved: *mut c_char,
        size: usize,
    ) -> *mut c_char {
        // SAFETY: forwarded from the caller.
        let bytes = unsafe { crate::string::as_bytes(path) };
        if !resolved.is_null() && bytes.len() + 1 > size {
            crate::chk_fail("realpath");
        }
        // SAFETY: checked above.
        unsafe { realpath(path, resolved) }
    }
}
