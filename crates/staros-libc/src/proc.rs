//! Layer 7 of the contract: the process, and the system it runs on.
//!
//! This is the layer where a C library stops describing computation and starts
//! describing the machine, and it is therefore the layer with the most opportunity
//! to lie. Most of POSIX's process interface assumes a Unix that is not here: there
//! is no `fork`, because an EL0 program cannot duplicate its own address space; no
//! `exec`, because the loader lives in the device manager and a program holds no
//! capability to it; no signal delivery, because nothing in this kernel interrupts a
//! task with a handler; no dynamic loader, because every program here is statically
//! linked.
//!
//! What this module does about that is the point of it. Three kinds of answer:
//!
//! * **Real.** The environment (a table this process owns), `getpid` (a syscall
//!   added for it, so two threads agree and two processes differ), `uname`,
//!   `getrlimit` (reporting limits the kernel actually enforces), `setjmp`/`longjmp`
//!   (AArch64 assembly, glibc's `jmp_buf` layout), `backtrace` (the frame-pointer
//!   chain, the same walk the kernel does for a fault), `ftok`, `sigemptyset` and
//!   its family (pure bit operations on a `sigset_t`).
//! * **True of this system.** One user, named root, with `/` for a home directory —
//!   `getpwuid_r` answers that because it *is* the answer here, not because a stub
//!   was easier. `getuid` is 0 for the same reason.
//! * **Refused, with the errno that says which.** `fork` is `ENOSYS`; `wait` is
//!   `ECHILD`; `dlopen` fails and `dlerror` explains why in a sentence; the System V
//!   IPC calls are `ENOSYS` because this system's shared memory is capabilities and
//!   its semaphores are futexes. A caller that checks — and these are all calls
//!   whose callers check — gets a fact rather than a plausible zero.
//!
//! The one that deserves its own note is `sigaction`. It succeeds, records the
//! disposition, and no signal is ever delivered to it, because nothing here can
//! raise one. Failing instead would be worse: Qt installs a `SIGPIPE` handler during
//! start-up and treats the failure as fatal, and there is no `SIGPIPE` on a system
//! with no pipes to break in the first place. The disposition is stored so
//! `sigaction`'s "give me the old one" half is honest, and [`kill`] to this process
//! is the one path that can reach it.

use core::ffi::c_char;

/// How many variables the environment holds. A fixed table rather than a growing
/// one: `environ` is a pointer C programs are entitled to keep across a `setenv`,
/// and a reallocating array would leave them holding freed memory. Sixty-four is
/// past what any program on this system sets.
const MAX_VARS: usize = 64;

/// The environment: `KEY=VALUE` strings, and the NULL-terminated pointer array C
/// reads them through.
///
/// It starts empty, which is the truth: there is no shell here to have exported
/// anything. A program that wants `QT_QPA_PLATFORM` set must set it, and one that
/// reads `HOME` gets nothing from here — [`exports::getpwuid_r`] is where the
/// answer to that question lives.
static mut VARS: [*mut c_char; MAX_VARS + 1] = [core::ptr::null_mut(); MAX_VARS + 1];
static mut VAR_COUNT: usize = 0;
static VARS_LOCK: crate::lock::Spin = crate::lock::Spin::new();

/// Split `KEY=VALUE` at the first `=`. A name with no `=` is the whole name.
fn key_of(entry: &[u8]) -> &[u8] {
    match entry.iter().position(|&c| c == b'=') {
        Some(cut) => &entry[..cut],
        None => entry,
    }
}

/// The index of `name` in the table, if it is there.
///
/// # Safety
/// Called under [`VARS_LOCK`]; the table's entries are NUL-terminated strings this
/// module allocated.
#[cfg(not(test))]
unsafe fn find(name: &[u8]) -> Option<usize> {
    // SAFETY: read under the lock; see `VARS`.
    let count = unsafe { VAR_COUNT };
    for i in 0..count {
        // SAFETY: entries `0..VAR_COUNT` are live strings.
        let entry = unsafe { crate::string::as_bytes(VARS[i]) };
        if key_of(entry) == name {
            return Some(i);
        }
    }
    None
}

/// A splitmix64 step. The mixing function behind [`exports::getentropy`], and it is
/// worth being plain about what that call is on this machine: there is no entropy
/// device, no interrupt-timing pool and no instruction that returns randomness, so
/// what comes back is a counter-driven sequence seeded from the monotonic clock. It
/// is good enough for what actually calls it — hash seeds, temporary names, the
/// `AT_RANDOM` bytes libstdc++ wants — and it is not a cryptographic source. A
/// program that needs one on this system does not have one, and would rather be told
/// that here than discover it later.
fn mix(state: u64) -> u64 {
    let mut z = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The C entry points.
#[cfg(not(test))]
pub mod exports {
    use super::{find, key_of, mix, MAX_VARS, VARS, VARS_LOCK, VAR_COUNT};
    use core::ffi::{c_char, c_int, c_void};

    use crate::fail;

    const EINVAL: c_int = 22;
    const ENOENT: c_int = 2;
    const ENOMEM: c_int = 12;
    const EPERM: c_int = 1;
    const ESRCH: c_int = 3;
    const ECHILD: c_int = 10;
    const ENOSYS: c_int = 38;
    const EACCES: c_int = 13;

    // ---------------------------------------------------------------- environment

    /// The array itself, under both names glibc exports it as. A program may read
    /// `environ` directly and walk it — that is a documented interface, not an
    /// implementation detail — which is why the table behind it is fixed-size.
    #[no_mangle]
    pub static mut environ: *mut *mut c_char = core::ptr::addr_of_mut!(VARS).cast();
    #[no_mangle]
    pub static mut __environ: *mut *mut c_char = core::ptr::addr_of_mut!(VARS).cast();

    /// # Safety
    /// C ABI: `name` is NUL-terminated.
    #[no_mangle]
    pub unsafe extern "C" fn getenv(name: *const c_char) -> *mut c_char {
        if name.is_null() {
            return core::ptr::null_mut();
        }
        let _guard = VARS_LOCK.lock();
        // SAFETY: forwarded from the caller.
        let wanted = unsafe { crate::string::as_bytes(name) };
        // SAFETY: under the lock.
        let Some(i) = (unsafe { find(wanted) }) else {
            return core::ptr::null_mut();
        };
        // SAFETY: entry `i` is a live `KEY=VALUE` string; the value starts after the
        // separator, and C wants a pointer *into* the stored string rather than a
        // copy — that is what makes `getenv`'s result valid until the next `setenv`.
        unsafe { VARS[i].add(wanted.len() + 1) }
    }

    /// `secure_getenv`: there is no privilege boundary to be careful about here —
    /// no set-user-id, no dynamic loader — so it is `getenv`.
    ///
    /// # Safety
    /// As [`getenv`].
    #[no_mangle]
    pub unsafe extern "C" fn secure_getenv(name: *const c_char) -> *mut c_char {
        // SAFETY: forwarded from the caller.
        unsafe { getenv(name) }
    }

    /// Store `KEY=VALUE` in a fresh allocation and put it at `slot`.
    ///
    /// # Safety
    /// Called under [`VARS_LOCK`]; `slot` is `<= VAR_COUNT` and within the table.
    unsafe fn store(slot: usize, name: &[u8], value: &[u8]) -> c_int {
        let len = name.len() + 1 + value.len() + 1;
        let mem = crate::heap_alloc(len, 1);
        if mem.is_null() {
            return fail(ENOMEM, -1);
        }
        // SAFETY: `mem` is `len` bytes and the three writes below cover it exactly.
        unsafe {
            core::ptr::copy_nonoverlapping(name.as_ptr(), mem, name.len());
            *mem.add(name.len()) = b'=';
            core::ptr::copy_nonoverlapping(value.as_ptr(), mem.add(name.len() + 1), value.len());
            *mem.add(len - 1) = 0;
        }
        // SAFETY: under the lock; replacing an entry frees the string it replaced,
        // which is why `getenv`'s result is only valid until the next `setenv` — as
        // C says.
        unsafe {
            if slot < VAR_COUNT && !VARS[slot].is_null() {
                crate::exports::free(VARS[slot].cast::<c_void>());
            }
            VARS[slot] = mem.cast::<c_char>();
            if slot == VAR_COUNT {
                VAR_COUNT += 1;
                VARS[VAR_COUNT] = core::ptr::null_mut();
            }
        }
        0
    }

    /// # Safety
    /// C ABI: both strings are NUL-terminated.
    #[no_mangle]
    pub unsafe extern "C" fn setenv(
        name: *const c_char,
        value: *const c_char,
        overwrite: c_int,
    ) -> c_int {
        if name.is_null() || value.is_null() {
            return fail(EINVAL, -1);
        }
        let _guard = VARS_LOCK.lock();
        // SAFETY: forwarded from the caller.
        let (name, value) = unsafe {
            (crate::string::as_bytes(name), crate::string::as_bytes(value))
        };
        // An empty name, or one containing `=`, is not a name: `setenv("A=B", …)`
        // would produce an entry no `getenv` could ever find again.
        if name.is_empty() || name.contains(&b'=') {
            return fail(EINVAL, -1);
        }
        // SAFETY: under the lock.
        match unsafe { find(name) } {
            Some(i) if overwrite == 0 => {
                let _ = i;
                0 // present, and the caller said not to replace it
            }
            // SAFETY: `i` indexes a live entry.
            Some(i) => unsafe { store(i, name, value) },
            None => {
                // SAFETY: under the lock.
                let count = unsafe { VAR_COUNT };
                if count >= MAX_VARS {
                    return fail(ENOMEM, -1);
                }
                // SAFETY: `count` is one past the last live entry.
                unsafe { store(count, name, value) }
            }
        }
    }

    /// # Safety
    /// C ABI: `name` is NUL-terminated.
    #[no_mangle]
    pub unsafe extern "C" fn unsetenv(name: *const c_char) -> c_int {
        if name.is_null() {
            return fail(EINVAL, -1);
        }
        let _guard = VARS_LOCK.lock();
        // SAFETY: forwarded from the caller.
        let name = unsafe { crate::string::as_bytes(name) };
        if name.is_empty() || name.contains(&b'=') {
            return fail(EINVAL, -1);
        }
        // SAFETY: under the lock.
        let Some(i) = (unsafe { find(name) }) else {
            return 0; // removing what is not there is success, as POSIX says
        };
        // SAFETY: under the lock. The last entry moves into the hole and the
        // terminator moves down, so the array stays contiguous and NULL-terminated —
        // which is what a program walking `environ` requires.
        unsafe {
            crate::exports::free(VARS[i].cast::<c_void>());
            VAR_COUNT -= 1;
            VARS[i] = VARS[VAR_COUNT];
            VARS[VAR_COUNT] = core::ptr::null_mut();
        }
        0
    }

    /// `putenv`: the caller's string *becomes* the entry, memory and all, which is
    /// the difference from `setenv` and the reason this one is easy to misuse. That
    /// is C's contract and not a choice made here.
    ///
    /// # Safety
    /// C ABI: `entry` is a NUL-terminated `KEY=VALUE` string that must outlive the
    /// call — it is not copied.
    #[no_mangle]
    pub unsafe extern "C" fn putenv(entry: *mut c_char) -> c_int {
        if entry.is_null() {
            return fail(EINVAL, -1);
        }
        let _guard = VARS_LOCK.lock();
        // SAFETY: forwarded from the caller.
        let bytes = unsafe { crate::string::as_bytes(entry) };
        let name = key_of(bytes);
        if name.is_empty() {
            return fail(EINVAL, -1);
        }
        // SAFETY: under the lock. The replaced entry is *not* freed: it may have
        // come from a previous `putenv` and belong to the caller.
        unsafe {
            match find(name) {
                Some(i) => VARS[i] = entry,
                None => {
                    if VAR_COUNT >= MAX_VARS {
                        return fail(ENOMEM, -1);
                    }
                    VARS[VAR_COUNT] = entry;
                    VAR_COUNT += 1;
                    VARS[VAR_COUNT] = core::ptr::null_mut();
                }
            }
        }
        0
    }

    /// `clearenv`: empty the table.
    #[no_mangle]
    pub extern "C" fn clearenv() -> c_int {
        let _guard = VARS_LOCK.lock();
        // SAFETY: under the lock. Entries are not freed for `putenv`'s sake — see
        // there — and leaking at most 64 short strings once is the lesser fault.
        unsafe {
            VAR_COUNT = 0;
            VARS[0] = core::ptr::null_mut();
        }
        0
    }

    // ------------------------------------------------------------------- identity

    /// This process's id. Every thread of it gets the same number, because the
    /// kernel carries the process identity separately from the scheduling one.
    #[no_mangle]
    pub extern "C" fn getpid() -> c_int {
        crate::sys::task_id() as c_int
    }

    /// The parent: every process here is started by the device manager, which is
    /// task 1 in every boot. Reporting it rather than 0 keeps `getppid() != 0` true,
    /// which is what a program checks when it wants to know if it is `init`.
    #[no_mangle]
    pub extern "C" fn getppid() -> c_int {
        1
    }

    /// One user, and it is root. Not a stub: there is no login, no password file and
    /// no privilege separation on this system, so every program runs as the only
    /// user there is.
    #[no_mangle]
    pub extern "C" fn getuid() -> c_int {
        0
    }

    #[no_mangle]
    pub extern "C" fn geteuid() -> c_int {
        0
    }

    #[no_mangle]
    pub extern "C" fn getgid() -> c_int {
        0
    }

    #[no_mangle]
    pub extern "C" fn getegid() -> c_int {
        0
    }

    /// Becoming a different user is meaningful only where there is one. Asking to
    /// become root succeeds because that is already true; anything else is refused
    /// rather than silently ignored, because a program that drops privilege and is
    /// told it worked will then do things it believes are safe.
    #[no_mangle]
    pub extern "C" fn setuid(uid: c_int) -> c_int {
        if uid == 0 { 0 } else { fail(EPERM, -1) }
    }

    #[no_mangle]
    pub extern "C" fn setgid(gid: c_int) -> c_int {
        if gid == 0 { 0 } else { fail(EPERM, -1) }
    }

    /// `setsid`: there are no sessions, no controlling terminal and no process
    /// groups, so this process is already the whole of its own session and its id is
    /// the answer.
    #[no_mangle]
    pub extern "C" fn setsid() -> c_int {
        getpid()
    }

    #[no_mangle]
    pub extern "C" fn getpgrp() -> c_int {
        getpid()
    }

    /// `struct passwd`, in glibc's layout.
    #[repr(C)]
    pub struct Passwd {
        pub pw_name: *mut c_char,
        pub pw_passwd: *mut c_char,
        pub pw_uid: u32,
        pub pw_gid: u32,
        pub pw_gecos: *mut c_char,
        pub pw_dir: *mut c_char,
        pub pw_shell: *mut c_char,
    }

    /// `struct group`, likewise.
    #[repr(C)]
    pub struct Group {
        pub gr_name: *mut c_char,
        pub gr_passwd: *mut c_char,
        pub gr_gid: u32,
        pub gr_mem: *mut *mut c_char,
    }

    /// The one user's fields. Static strings, copied into the caller's buffer the
    /// way the `_r` interface requires.
    const USER: &[u8] = b"root\0";
    const NO_PASSWORD: &[u8] = b"*\0";
    const HOME: &[u8] = b"/\0";
    const SHELL: &[u8] = b"/bin/sh\0";

    /// Copy `src` into `buf` at `at`, returning the pointer to it and the new
    /// offset, or `None` if it does not fit.
    ///
    /// # Safety
    /// `buf` is valid for `len` bytes.
    unsafe fn park(buf: *mut c_char, len: usize, at: usize, src: &[u8]) -> Option<(*mut c_char, usize)> {
        if at + src.len() > len {
            return None;
        }
        // SAFETY: bounds checked immediately above.
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), buf.add(at).cast::<u8>(), src.len());
            Some((buf.add(at), at + src.len()))
        }
    }

    /// The user database, which on this system has one row.
    ///
    /// Qt calls this to find a home directory before it will start. Answering with
    /// `/` is not a placeholder — it is where a program's files are on a machine
    /// whose whole filesystem is one read-only archive.
    ///
    /// # Safety
    /// C ABI: `buf` is valid for `len` bytes, `out` and `result` for one pointer.
    #[no_mangle]
    pub unsafe extern "C" fn getpwuid_r(
        uid: u32,
        out: *mut Passwd,
        buf: *mut c_char,
        len: usize,
        result: *mut *mut Passwd,
    ) -> c_int {
        if !result.is_null() {
            // SAFETY: the caller's contract. Cleared first: POSIX says "not found"
            // is a null result with a zero return, so a caller that ignores the
            // return value must still see the truth.
            unsafe { *result = core::ptr::null_mut() };
        }
        if out.is_null() || buf.is_null() {
            return EINVAL;
        }
        if uid != 0 {
            return 0; // no such user, which is not an error
        }
        // SAFETY: the caller's contract.
        let Some((name, at)) = (unsafe { park(buf, len, 0, USER) }) else {
            return 34; // ERANGE
        };
        // SAFETY: as above, each write bounded by `len`.
        let Some((passwd, at)) = (unsafe { park(buf, len, at, NO_PASSWORD) }) else {
            return 34;
        };
        // SAFETY: as above.
        let Some((dir, at)) = (unsafe { park(buf, len, at, HOME) }) else {
            return 34;
        };
        // SAFETY: as above.
        let Some((shell, _)) = (unsafe { park(buf, len, at, SHELL) }) else {
            return 34;
        };
        // SAFETY: the caller's contract; `out` is one `Passwd`.
        unsafe {
            out.write(Passwd {
                pw_name: name,
                pw_passwd: passwd,
                pw_uid: 0,
                pw_gid: 0,
                pw_gecos: name,
                pw_dir: dir,
                pw_shell: shell,
            });
            if !result.is_null() {
                *result = out;
            }
        }
        0
    }

    /// The group database, which has the same one row.
    ///
    /// # Safety
    /// As [`getpwuid_r`].
    #[no_mangle]
    pub unsafe extern "C" fn getgrgid_r(
        gid: u32,
        out: *mut Group,
        buf: *mut c_char,
        len: usize,
        result: *mut *mut Group,
    ) -> c_int {
        if !result.is_null() {
            // SAFETY: the caller's contract.
            unsafe { *result = core::ptr::null_mut() };
        }
        if out.is_null() || buf.is_null() {
            return EINVAL;
        }
        if gid != 0 {
            return 0;
        }
        // SAFETY: the caller's contract.
        let Some((name, at)) = (unsafe { park(buf, len, 0, USER) }) else {
            return 34; // ERANGE
        };
        // SAFETY: as above.
        let Some((passwd, at)) = (unsafe { park(buf, len, at, NO_PASSWORD) }) else {
            return 34;
        };
        // The member list is a NULL-terminated array of pointers, and an empty one
        // is still an array: it needs a NULL in the caller's buffer to point at.
        let members = at.next_multiple_of(8);
        if members + 8 > len {
            return 34;
        }
        // SAFETY: bounds checked; eight aligned bytes inside `buf`.
        unsafe {
            let slot = buf.add(members).cast::<*mut c_char>();
            slot.write(core::ptr::null_mut());
            out.write(Group { gr_name: name, gr_passwd: passwd, gr_gid: 0, gr_mem: slot });
            if !result.is_null() {
                *result = out;
            }
        }
        0
    }

    /// The static buffer the non-reentrant lookups hand out.
    ///
    /// One per function, 128 bytes, which is more than the four short strings need.
    static mut PASSWD_BUF: [c_char; 128] = [0; 128];
    static mut PASSWD_ROW: Passwd = Passwd {
        pw_name: core::ptr::null_mut(),
        pw_passwd: core::ptr::null_mut(),
        pw_uid: 0,
        pw_gid: 0,
        pw_gecos: core::ptr::null_mut(),
        pw_dir: core::ptr::null_mut(),
        pw_shell: core::ptr::null_mut(),
    };

    /// `getpwuid`: the same one row, into storage this library owns.
    ///
    /// The `_r` form was written first and this is deliberately built on top of it
    /// rather than beside it, so there is one place that decides what the row says.
    ///
    /// It is the interface every style guide warns about — the returned pointer is
    /// into a static buffer that the next call overwrites — and it is here because
    /// Qt calls it: `qfilesystemengine_unix.cpp` line 827, for `QFileInfo::owner()`.
    /// The race it is famous for cannot bite quite as hard here as it would
    /// elsewhere, because the answer never changes: two threads racing overwrite the
    /// buffer with identical bytes. That is an argument for why this is *tolerable*,
    /// not for why it is *good*, and a caller that can use `getpwuid_r` should.
    ///
    /// # Safety
    /// C ABI. The returned pointer is invalidated by the next call from any thread.
    #[no_mangle]
    pub unsafe extern "C" fn getpwuid(uid: u32) -> *mut Passwd {
        let mut result = core::ptr::null_mut();
        // SAFETY: the statics are this function's own storage, and `getpwuid_r`
        // writes only inside the bounds given.
        unsafe {
            let row = &raw mut PASSWD_ROW;
            let buf = (&raw mut PASSWD_BUF).cast::<c_char>();
            if getpwuid_r(uid, row, buf, 128, &raw mut result) != 0 {
                return core::ptr::null_mut();
            }
        }
        result
    }

    static mut GROUP_BUF: [c_char; 128] = [0; 128];
    static mut GROUP_ROW: Group = Group {
        gr_name: core::ptr::null_mut(),
        gr_passwd: core::ptr::null_mut(),
        gr_gid: 0,
        gr_mem: core::ptr::null_mut(),
    };

    /// `getgrgid`: as [`getpwuid`], for the group database. Qt calls it from the same
    /// file, line 866, for `QFileInfo::group()`.
    ///
    /// # Safety
    /// As [`getpwuid`].
    #[no_mangle]
    pub unsafe extern "C" fn getgrgid(gid: u32) -> *mut Group {
        let mut result = core::ptr::null_mut();
        // SAFETY: as `getpwuid`.
        unsafe {
            let row = &raw mut GROUP_ROW;
            let buf = (&raw mut GROUP_BUF).cast::<c_char>();
            if getgrgid_r(gid, row, buf, 128, &raw mut result) != 0 {
                return core::ptr::null_mut();
            }
        }
        result
    }

    // --------------------------------------------------------------------- system

    /// `struct utsname`, in glibc's layout: six 65-byte fields.
    #[repr(C)]
    pub struct Utsname {
        pub sysname: [c_char; 65],
        pub nodename: [c_char; 65],
        pub release: [c_char; 65],
        pub version: [c_char; 65],
        pub machine: [c_char; 65],
        pub domainname: [c_char; 65],
    }

    const _: () = {
        assert!(core::mem::size_of::<Utsname>() == 390);
    };

    /// Copy a NUL-terminated name into one `utsname` field.
    fn name_into(field: &mut [c_char; 65], text: &[u8]) {
        for (slot, &b) in field.iter_mut().zip(text.iter()) {
            *slot = b as c_char;
        }
        field[text.len().min(64)] = 0;
    }

    /// # Safety
    /// C ABI: `out` is valid for one `Utsname`.
    #[no_mangle]
    pub unsafe extern "C" fn uname(out: *mut Utsname) -> c_int {
        if out.is_null() {
            return fail(EINVAL, -1);
        }
        // SAFETY: the caller's contract. Written whole: a partially filled
        // `utsname` has uninitialised bytes where a program expects a string.
        unsafe { core::ptr::write_bytes(out, 0, 1) };
        // SAFETY: as above.
        let u = unsafe { &mut *out };
        name_into(&mut u.sysname, b"StarOS");
        name_into(&mut u.nodename, b"staros");
        name_into(&mut u.release, b"0.2.0");
        // The version string says what this actually is, because a program printing
        // it is showing a person who wants to know.
        name_into(&mut u.version, b"StarOS microkernel, EL0 user space");
        name_into(&mut u.machine, b"aarch64");
        name_into(&mut u.domainname, b"(none)");
        0
    }

    /// `sysconf`, for the questions with an answer here.
    const SC_ARG_MAX: c_int = 0;
    const SC_PAGESIZE: c_int = 30;
    const SC_NPROCESSORS_CONF: c_int = 83;
    const SC_NPROCESSORS_ONLN: c_int = 84;
    const SC_OPEN_MAX: c_int = 4;
    const SC_CLK_TCK: c_int = 2;

    /// The auxiliary vector's entries, as `getauxval` names them.
    const AT_CLKTCK: u64 = 17;
    const AT_PAGESZ: u64 = 6;
    const AT_HWCAP: u64 = 16;
    const AT_SECURE: u64 = 23;
    const AT_RANDOM: u64 = 25;
    const AT_UID: u64 = 11;
    const AT_EUID: u64 = 12;
    const AT_GID: u64 = 13;
    const AT_EGID: u64 = 14;

    /// Sixteen bytes for `AT_RANDOM`, filled on first use.
    ///
    /// glibc's stack protector and libstdc++'s hash seed both take their value from
    /// this pointer, and both want it to be stable for the life of the process —
    /// which is why it is a static filled once rather than a fresh draw per call.
    static mut RANDOM_BYTES: [u8; 16] = [0; 16];
    static mut RANDOM_READY: bool = false;
    static RANDOM_LOCK: crate::lock::Spin = crate::lock::Spin::new();

    /// The running counter behind [`getentropy`].
    static mut ENTROPY_STATE: u64 = 0;

    /// Draw the next value. See [`super::mix`] for what this is and is not.
    fn next_random() -> u64 {
        // SAFETY: under `RANDOM_LOCK`, which every caller takes.
        unsafe {
            if ENTROPY_STATE == 0 {
                // The clock is the only thing on this machine that differs between
                // one boot and the next. Mixed with the address of the state itself,
                // which differs between processes.
                let seed = crate::sys::clock_now().unwrap_or(0)
                    ^ (core::ptr::addr_of!(ENTROPY_STATE) as u64);
                ENTROPY_STATE = mix(seed | 1);
            }
            ENTROPY_STATE = mix(ENTROPY_STATE);
            ENTROPY_STATE
        }
    }

    /// # Safety
    /// C ABI: `buf` is valid for `len` bytes.
    #[no_mangle]
    pub unsafe extern "C" fn getentropy(buf: *mut c_void, len: usize) -> c_int {
        // POSIX caps a single request at 256 bytes and callers rely on the refusal.
        if buf.is_null() || len > 256 {
            return fail(EINVAL, -1);
        }
        let _guard = RANDOM_LOCK.lock();
        let mut done = 0;
        while done < len {
            let word = next_random().to_ne_bytes();
            let take = (len - done).min(8);
            // SAFETY: the caller's contract; `done + take <= len`.
            unsafe { core::ptr::copy_nonoverlapping(word.as_ptr(), buf.cast::<u8>().add(done), take) };
            done += take;
        }
        0
    }

    /// `getrandom`, the other spelling. The flags ask about blocking on an entropy
    /// pool that does not exist, so they change nothing.
    ///
    /// # Safety
    /// As [`getentropy`].
    #[no_mangle]
    pub unsafe extern "C" fn getrandom(buf: *mut c_void, len: usize, _flags: u32) -> isize {
        let mut done = 0;
        while done < len {
            let take = (len - done).min(256);
            // SAFETY: forwarded from the caller.
            if unsafe { getentropy(buf.cast::<u8>().add(done).cast(), take) } != 0 {
                return -1;
            }
            done += take;
        }
        done as isize
    }

    /// The auxiliary vector.
    ///
    /// There is no real one — the kernel `eret`s into `_start` with a stack and
    /// nothing on it — so this answers the entries whose values are facts about the
    /// system and reports the rest as absent, which is exactly what glibc's
    /// `getauxval` does for an entry the loader did not supply.
    #[no_mangle]
    pub extern "C" fn getauxval(kind: u64) -> u64 {
        match kind {
            AT_PAGESZ => 4096,
            AT_CLKTCK => 100,
            AT_SECURE => 0,
            AT_UID | AT_EUID | AT_GID | AT_EGID => 0,
            // Reporting no optional features is the safe direction: a program that
            // believes in a feature this CPU lacks executes an undefined instruction,
            // and one that does not use a feature it has is merely slower.
            AT_HWCAP => 0,
            AT_RANDOM => {
                let _guard = RANDOM_LOCK.lock();
                // SAFETY: under the lock.
                unsafe {
                    if !RANDOM_READY {
                        let bytes = &mut *core::ptr::addr_of_mut!(RANDOM_BYTES);
                        for chunk in bytes.chunks_mut(8) {
                            chunk.copy_from_slice(&next_random().to_ne_bytes()[..chunk.len()]);
                        }
                        RANDOM_READY = true;
                    }
                    core::ptr::addr_of!(RANDOM_BYTES) as u64
                }
            }
            _ => fail(ENOENT, 0),
        }
    }

    #[no_mangle]
    pub extern "C" fn sysconf(name: c_int) -> i64 {
        match name {
            SC_PAGESIZE => 4096,
            SC_CLK_TCK => 100,
            SC_OPEN_MAX => crate::fd::MAX_FDS as i64,
            SC_ARG_MAX => 4096,
            // One core, from this process's point of view: the kernel schedules
            // across every core it found, but nothing here can ask it how many, and
            // a number this library invented would be worse than a conservative one.
            SC_NPROCESSORS_CONF | SC_NPROCESSORS_ONLN => 1,
            _ => fail(EINVAL, -1),
        }
    }

    /// `struct rlimit`.
    #[repr(C)]
    pub struct Rlimit {
        pub rlim_cur: u64,
        pub rlim_max: u64,
    }

    const RLIMIT_STACK: c_int = 3;
    const RLIMIT_NOFILE: c_int = 7;
    const RLIMIT_AS: c_int = 9;
    const RLIM_INFINITY: u64 = u64::MAX;

    /// The limits this system actually has.
    ///
    /// `RLIMIT_STACK` is the real number: the kernel grows a task's stack on demand
    /// and stops at `USER_STACK_MAX_PAGES`, and a program that sizes a recursion or
    /// an `alloca` from this value is entitled to the truth rather than to
    /// `RLIM_INFINITY`. The descriptor limit is likewise the table's real size.
    ///
    /// The number is `crate::thread::MAIN_STACK_SIZE`, which is a copy of the
    /// kernel's constant — see the comment there for why a C library cannot import
    /// it and where the other side is.
    ///
    /// # Safety
    /// C ABI: `out` is valid for one `Rlimit`.
    #[no_mangle]
    pub unsafe extern "C" fn getrlimit(resource: c_int, out: *mut Rlimit) -> c_int {
        if out.is_null() {
            return fail(EINVAL, -1);
        }
        let (cur, max) = match resource {
            RLIMIT_STACK => (
                crate::thread::MAIN_STACK_SIZE as u64,
                crate::thread::MAIN_STACK_SIZE as u64,
            ),
            RLIMIT_NOFILE => (crate::fd::MAX_OPEN as u64, crate::fd::MAX_OPEN as u64),
            RLIMIT_AS => (RLIM_INFINITY, RLIM_INFINITY),
            _ => (RLIM_INFINITY, RLIM_INFINITY),
        };
        // SAFETY: the caller's contract.
        unsafe { out.write(Rlimit { rlim_cur: cur, rlim_max: max }) };
        0
    }

    /// # Safety
    /// As [`getrlimit`].
    #[no_mangle]
    pub unsafe extern "C" fn getrlimit64(resource: c_int, out: *mut Rlimit) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { getrlimit(resource, out) }
    }

    /// Limits are properties of the kernel's design here, not settings. Asking for
    /// what is already true succeeds; asking to change it is refused, because a
    /// program told its stack limit is now 8 MiB will recurse into the guard page.
    ///
    /// # Safety
    /// C ABI: `limit` is valid for one `Rlimit`.
    #[no_mangle]
    pub unsafe extern "C" fn setrlimit(resource: c_int, limit: *const Rlimit) -> c_int {
        if limit.is_null() {
            return fail(EINVAL, -1);
        }
        let mut current = Rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: `current` is one initialised `Rlimit`.
        if unsafe { getrlimit(resource, &raw mut current) } != 0 {
            return -1;
        }
        // SAFETY: the caller's contract.
        let asked = unsafe { &*limit };
        if asked.rlim_cur <= current.rlim_cur && asked.rlim_max <= current.rlim_max {
            // Lowering is honoured in the only sense available: nothing here raises
            // the limit, so a program that lowers one is already obeyed.
            return 0;
        }
        fail(EPERM, -1)
    }

    /// # Safety
    /// As [`setrlimit`].
    #[no_mangle]
    pub unsafe extern "C" fn setrlimit64(resource: c_int, limit: *const Rlimit) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { setrlimit(resource, limit) }
    }

    /// The process name `prctl` can set and get. Sixteen bytes including the NUL, as
    /// Linux defines it.
    static mut PROC_NAME: [u8; 16] = *b"program\0\0\0\0\0\0\0\0\0";

    const PR_SET_NAME: c_int = 15;
    const PR_GET_NAME: c_int = 16;

    /// `prctl`, for the two requests that mean something here.
    ///
    /// The name is real state — set it and get it back — because that is all
    /// `PR_SET_NAME` ever was. Everything else concerns machinery this kernel does
    /// not have (seccomp, capabilities, the OOM killer, `PDEATHSIG`), and `EINVAL`
    /// is what Linux itself returns for a request it does not implement.
    ///
    /// # Safety
    /// C ABI, variadic: `PR_SET_NAME` and `PR_GET_NAME` take a pointer to at least
    /// 16 bytes.
    #[no_mangle]
    pub unsafe extern "C" fn prctl(option: c_int, mut args: ...) -> c_int {
        match option {
            PR_SET_NAME => {
                // SAFETY: the caller's contract for this option.
                let src = unsafe { args.next_arg::<*const c_char>() };
                if src.is_null() {
                    return fail(EINVAL, -1);
                }
                // SAFETY: as above.
                let bytes = unsafe { crate::string::as_bytes(src) };
                // SAFETY: a static array; the copy is bounded by both lengths and
                // the last byte is left as the terminator.
                unsafe {
                    let name = &mut *core::ptr::addr_of_mut!(PROC_NAME);
                    name.fill(0);
                    let n = bytes.len().min(15);
                    name[..n].copy_from_slice(&bytes[..n]);
                }
                0
            }
            PR_GET_NAME => {
                // SAFETY: the caller's contract for this option.
                let dst = unsafe { args.next_arg::<*mut c_char>() };
                if dst.is_null() {
                    return fail(EINVAL, -1);
                }
                // SAFETY: the caller promised 16 bytes.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        core::ptr::addr_of!(PROC_NAME).cast::<c_char>(),
                        dst,
                        16,
                    );
                }
                0
            }
            _ => fail(EINVAL, -1),
        }
    }

    /// The raw `syscall` interface.
    ///
    /// This one is refused rather than forwarded, and the reason is worth stating:
    /// the numbers a caller passes here are *Linux's*, and this kernel's are its own
    /// (`crates/abi/src/syscall.rs`). Forwarding would call `Revoke` when a program
    /// asked for `gettid`. `ENOSYS` is the truthful answer for every number, and it
    /// is one that callers of this interface always check, since it is the interface
    /// used precisely when a wrapper might be missing.
    ///
    /// # Safety
    /// C ABI, variadic.
    #[no_mangle]
    pub unsafe extern "C" fn syscall(_number: i64, mut _args: ...) -> i64 {
        fail(ENOSYS, -1)
    }

    // -------------------------------------------------------------------- signals

    /// `sigset_t`: 1024 bits, as glibc defines it.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Sigset {
        pub bits: [u64; 16],
    }

    /// `struct sigaction`, in glibc's layout — the handler first, the mask next, and
    /// `sa_flags` after 128 bytes of it.
    #[repr(C)]
    pub struct Sigaction {
        pub sa_handler: *mut c_void,
        pub sa_mask: Sigset,
        pub sa_flags: c_int,
        pub sa_restorer: *mut c_void,
    }

    const _: () = {
        assert!(core::mem::size_of::<Sigset>() == 128);
        assert!(core::mem::offset_of!(Sigaction, sa_mask) == 8);
        assert!(core::mem::offset_of!(Sigaction, sa_flags) == 136);
        assert!(core::mem::size_of::<Sigaction>() == 152);
    };

    /// The highest signal number with a name, and the size of the table below.
    const NSIG: usize = 65;

    /// The dispositions this process has installed. Recorded, never delivered — see
    /// the module's note.
    static mut HANDLERS: [*mut c_void; NSIG] = [core::ptr::null_mut(); NSIG];
    static SIGNAL_LOCK: crate::lock::Spin = crate::lock::Spin::new();

    /// The blocked mask. Kept for the same reason as the handlers: `sigprocmask`'s
    /// "give me the old mask" half is a real question with a real answer.
    static mut BLOCKED: Sigset = Sigset { bits: [0; 16] };

    /// The bit operations are real arithmetic and are implemented as such — a
    /// program that builds a mask and reads it back gets what it built.
    ///
    /// # Safety
    /// C ABI: `set` is valid for one `Sigset`.
    #[no_mangle]
    pub unsafe extern "C" fn sigemptyset(set: *mut Sigset) -> c_int {
        if set.is_null() {
            return fail(EINVAL, -1);
        }
        // SAFETY: the caller's contract.
        unsafe { set.write(Sigset { bits: [0; 16] }) };
        0
    }

    /// # Safety
    /// As [`sigemptyset`].
    #[no_mangle]
    pub unsafe extern "C" fn sigfillset(set: *mut Sigset) -> c_int {
        if set.is_null() {
            return fail(EINVAL, -1);
        }
        // SAFETY: the caller's contract.
        unsafe { set.write(Sigset { bits: [u64::MAX; 16] }) };
        0
    }

    /// Which word and which bit signal `n` is. Signals are numbered from 1, so bit
    /// zero is signal one — an off-by-one here makes `sigaddset(SIGKILL)` set
    /// `SIGTERM`.
    fn bit_of(n: c_int) -> Option<(usize, u64)> {
        if n < 1 || n as usize >= NSIG {
            return None;
        }
        let index = (n - 1) as usize;
        Some((index / 64, 1u64 << (index % 64)))
    }

    /// # Safety
    /// As [`sigemptyset`].
    #[no_mangle]
    pub unsafe extern "C" fn sigaddset(set: *mut Sigset, signal: c_int) -> c_int {
        let Some((word, mask)) = bit_of(signal) else {
            return fail(EINVAL, -1);
        };
        if set.is_null() {
            return fail(EINVAL, -1);
        }
        // SAFETY: the caller's contract.
        unsafe { (*set).bits[word] |= mask };
        0
    }

    /// # Safety
    /// As [`sigemptyset`].
    #[no_mangle]
    pub unsafe extern "C" fn sigdelset(set: *mut Sigset, signal: c_int) -> c_int {
        let Some((word, mask)) = bit_of(signal) else {
            return fail(EINVAL, -1);
        };
        if set.is_null() {
            return fail(EINVAL, -1);
        }
        // SAFETY: the caller's contract.
        unsafe { (*set).bits[word] &= !mask };
        0
    }

    /// # Safety
    /// As [`sigemptyset`].
    #[no_mangle]
    pub unsafe extern "C" fn sigismember(set: *const Sigset, signal: c_int) -> c_int {
        let Some((word, mask)) = bit_of(signal) else {
            return fail(EINVAL, -1);
        };
        if set.is_null() {
            return fail(EINVAL, -1);
        }
        // SAFETY: the caller's contract.
        c_int::from(unsafe { (*set).bits[word] & mask != 0 })
    }

    /// Install a disposition, and report the one it replaced.
    ///
    /// Succeeds, and no signal is ever delivered to what it installs, because
    /// nothing in this kernel raises one. See the module's note for why succeeding
    /// is the right answer and failing is not.
    ///
    /// # Safety
    /// C ABI: `new` and `old` are null or valid for one `Sigaction`.
    #[no_mangle]
    pub unsafe extern "C" fn sigaction(
        signal: c_int,
        new: *const Sigaction,
        old: *mut Sigaction,
    ) -> c_int {
        let Some(_) = bit_of(signal) else {
            return fail(EINVAL, -1);
        };
        let slot = signal as usize;
        let _guard = SIGNAL_LOCK.lock();
        // SAFETY: under the lock.
        let previous = unsafe { HANDLERS[slot] };
        if !old.is_null() {
            // SAFETY: the caller's contract.
            unsafe {
                old.write(Sigaction {
                    sa_handler: previous,
                    sa_mask: Sigset { bits: [0; 16] },
                    sa_flags: 0,
                    sa_restorer: core::ptr::null_mut(),
                });
            }
        }
        if !new.is_null() {
            // SAFETY: the caller's contract, under the lock.
            unsafe { HANDLERS[slot] = (*new).sa_handler };
        }
        0
    }

    /// `signal`, the older interface, over the same table.
    ///
    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn signal(number: c_int, handler: *mut c_void) -> *mut c_void {
        let Some(_) = bit_of(number) else {
            return fail(EINVAL, usize::MAX as *mut c_void); // SIG_ERR
        };
        let _guard = SIGNAL_LOCK.lock();
        // SAFETY: under the lock.
        unsafe {
            let previous = HANDLERS[number as usize];
            HANDLERS[number as usize] = handler;
            previous
        }
    }

    const SIG_BLOCK: c_int = 0;
    const SIG_UNBLOCK: c_int = 1;
    const SIG_SETMASK: c_int = 2;

    /// # Safety
    /// C ABI: both sets are null or valid for one `Sigset`.
    #[no_mangle]
    pub unsafe extern "C" fn sigprocmask(
        how: c_int,
        set: *const Sigset,
        old: *mut Sigset,
    ) -> c_int {
        let _guard = SIGNAL_LOCK.lock();
        // SAFETY: under the lock.
        let current = unsafe { BLOCKED };
        if !old.is_null() {
            // SAFETY: the caller's contract.
            unsafe { old.write(current) };
        }
        if set.is_null() {
            return 0;
        }
        // SAFETY: the caller's contract.
        let asked = unsafe { *set };
        let mut next = current;
        match how {
            SIG_BLOCK => {
                for i in 0..16 {
                    next.bits[i] |= asked.bits[i];
                }
            }
            SIG_UNBLOCK => {
                for i in 0..16 {
                    next.bits[i] &= !asked.bits[i];
                }
            }
            SIG_SETMASK => next = asked,
            _ => return fail(EINVAL, -1),
        }
        // SAFETY: under the lock.
        unsafe { BLOCKED = next };
        0
    }

    /// `pthread_sigmask`: one mask for the process, so it is `sigprocmask`.
    ///
    /// # Safety
    /// As [`sigprocmask`].
    #[no_mangle]
    pub unsafe extern "C" fn pthread_sigmask(
        how: c_int,
        set: *const Sigset,
        old: *mut Sigset,
    ) -> c_int {
        // SAFETY: forwarded from the caller.
        unsafe { sigprocmask(how, set, old) }
    }

    /// Signals whose default action ends the process. `kill` to this process with
    /// one of them does exactly that, which makes the call real rather than
    /// theoretical.
    fn is_fatal(signal: c_int) -> bool {
        matches!(signal, 2 | 3 | 4 | 6 | 8 | 9 | 11 | 13 | 15)
    }

    /// `kill`.
    ///
    /// To this process, with a fatal signal and no handler installed, it does what
    /// the default action says: the process ends with status 128 + the signal, which
    /// is the encoding a shell would report. To any other process it is `ESRCH` —
    /// not because that process does not exist, but because this one holds no
    /// capability naming it, and an id is not authority in this system.
    #[no_mangle]
    pub extern "C" fn kill(pid: c_int, signal: c_int) -> c_int {
        if bit_of(signal).is_none() && signal != 0 {
            return fail(EINVAL, -1);
        }
        if pid != getpid() {
            return fail(ESRCH, -1);
        }
        if signal == 0 {
            return 0; // the existence check, and this process exists
        }
        let _guard = SIGNAL_LOCK.lock();
        // SAFETY: under the lock.
        let handler = unsafe { HANDLERS[signal as usize] };
        // 0 is SIG_DFL and 1 is SIG_IGN; anything else is a handler this system
        // cannot call, and calling it from here would run it on the wrong stack with
        // no signal frame under it.
        if handler.is_null() && is_fatal(signal) {
            drop(_guard);
            crate::exports::exit(128 + signal);
        }
        0
    }

    /// `raise`: `kill` to oneself, which is the whole definition.
    #[no_mangle]
    pub extern "C" fn raise(signal: c_int) -> c_int {
        kill(getpid(), signal)
    }

    // ------------------------------------------------------- processes that are not

    /// `fork`.
    ///
    /// Not refused out of laziness: duplicating an address space means copying page
    /// tables, and an EL0 program holds no capability that names its own. Creating a
    /// process here is `Spawn`, which takes an image rather than a copy of the
    /// caller — a different operation with a different meaning, and pretending one is
    /// the other would give a caller a child that shares nothing it expects to share.
    #[no_mangle]
    pub extern "C" fn fork() -> c_int {
        fail(ENOSYS, -1)
    }

    #[no_mangle]
    pub extern "C" fn vfork() -> c_int {
        fail(ENOSYS, -1)
    }

    /// # Safety
    /// C ABI, variadic.
    #[no_mangle]
    pub unsafe extern "C" fn clone(_fn: *mut c_void, _stack: *mut c_void, _flags: c_int, mut _args: ...) -> c_int {
        // A thread is `pthread_create`, which this library implements over
        // `SpawnThread`. `clone`'s other uses are namespaces and address-space
        // sharing tricks that have no counterpart here.
        fail(ENOSYS, -1)
    }

    /// Replacing this program's image needs the loader, which lives in the device
    /// manager and is reached with a capability this process does not hold.
    /// `EACCES` says that: the file may well exist, and this process may not run it.
    ///
    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn execv(_path: *const c_char, _argv: *const *const c_char) -> c_int {
        fail(EACCES, -1)
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn execve(
        _path: *const c_char,
        _argv: *const *const c_char,
        _envp: *const *const c_char,
    ) -> c_int {
        fail(EACCES, -1)
    }

    /// # Safety
    /// C ABI, variadic.
    #[no_mangle]
    pub unsafe extern "C" fn execl(_path: *const c_char, mut _args: ...) -> c_int {
        fail(EACCES, -1)
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn execvp(_file: *const c_char, _argv: *const *const c_char) -> c_int {
        fail(EACCES, -1)
    }

    /// Waiting for a child, of which this process has none — it cannot make one.
    /// `ECHILD` is precisely that statement and is what a `wait` loop terminates on.
    ///
    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn waitpid(_pid: c_int, _status: *mut c_int, _options: c_int) -> c_int {
        fail(ECHILD, -1)
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn wait(_status: *mut c_int) -> c_int {
        fail(ECHILD, -1)
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn wait4(
        _pid: c_int,
        _status: *mut c_int,
        _options: c_int,
        _usage: *mut c_void,
    ) -> c_int {
        fail(ECHILD, -1)
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn waitid(
        _kind: c_int,
        _id: c_int,
        _info: *mut c_void,
        _options: c_int,
    ) -> c_int {
        fail(ECHILD, -1)
    }

    /// `system`: running a command needs a shell, and there is none.
    ///
    /// The return value says which of two things happened, and this is the one that
    /// means "no shell is available" — not "the command failed".
    ///
    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn system(command: *const c_char) -> c_int {
        if command.is_null() {
            return 0; // "is there a shell?" — no
        }
        fail(ENOSYS, -1)
    }

    // ------------------------------------------------------------ dynamic loading

    /// The message [`dlerror`] returns. One sentence, because it is printed.
    const NO_LOADER: &[u8] = b"dynamic loading is not available: every program on this system is statically linked\0";

    /// Whether a `dl*` call has failed since the last [`dlerror`]. C says the error
    /// is consumed by reading it, and a program that polls in a loop must see NULL
    /// the second time.
    static mut DL_FAILED: bool = false;

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn dlopen(_path: *const c_char, _flags: c_int) -> *mut c_void {
        // SAFETY: a single flag; the same reasoning as `ERRNO`.
        unsafe { DL_FAILED = true };
        core::ptr::null_mut()
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn dlsym(_handle: *mut c_void, _symbol: *const c_char) -> *mut c_void {
        // SAFETY: as `dlopen`.
        unsafe { DL_FAILED = true };
        core::ptr::null_mut()
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn dlclose(_handle: *mut c_void) -> c_int {
        0 // closing what was never opened is not an error
    }

    #[no_mangle]
    pub extern "C" fn dlerror() -> *const c_char {
        // SAFETY: a single flag.
        unsafe {
            if !DL_FAILED {
                return core::ptr::null();
            }
            DL_FAILED = false;
        }
        NO_LOADER.as_ptr().cast::<c_char>()
    }

    /// `dladdr`: which shared object an address came from. There is one object, this
    /// program, and no symbol table to search — so the answer is 0, which `dladdr`
    /// defines as "not found" rather than as an error.
    ///
    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn dladdr(_addr: *const c_void, _info: *mut c_void) -> c_int {
        0
    }

    // ------------------------------------------------------------- System V IPC

    /// `ftok`: a key from a path and a project byte. Pure computation over what
    /// `stat` reports, so it is implemented rather than refused — even though what
    /// the key would be *used* for is not available here.
    ///
    /// # Safety
    /// C ABI: `path` is NUL-terminated.
    #[no_mangle]
    pub unsafe extern "C" fn ftok(path: *const c_char, project: c_int) -> c_int {
        let mut st = crate::file::exports::Stat::default();
        // SAFETY: forwarded from the caller; `st` is one initialised `Stat`.
        if unsafe { crate::file::exports::stat(path, &raw mut st) } != 0 {
            return -1;
        }
        // The classic construction: the project byte on top, the device in the
        // middle, the inode below.
        let key = ((project as u32 & 0xff) << 24)
            | ((st.st_dev as u32 & 0xff) << 16)
            | (st.st_ino as u32 & 0xffff);
        key as c_int
    }

    /// System V shared memory and semaphores.
    ///
    /// This system has both of the things they are for, under different names and
    /// with different rules: shared memory is a capability delegated over IPC
    /// (`CreateShared`/`MapShared`), and a semaphore is a futex over
    /// `NotifySignal`/`Wait`. Neither is reachable through a global numeric key,
    /// because a key is not a capability — any process that guessed the number would
    /// have the memory. `ENOSYS` is the honest report: not "this failed" but "this
    /// interface does not exist here".
    macro_rules! sysv {
        ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty = $bad:expr),* $(,)?) => {$(
            /// # Safety
            /// C ABI.
            #[no_mangle]
            pub unsafe extern "C" fn $name($($arg: $ty),*) -> $ret {
                $(let _ = $arg;)*
                fail(ENOSYS, $bad)
            }
        )*};
    }

    sysv! {
        shmget(key: c_int, size: usize, flags: c_int) -> c_int = -1,
        shmat(id: c_int, addr: *const c_void, flags: c_int) -> *mut c_void = usize::MAX as *mut c_void,
        shmdt(addr: *const c_void) -> c_int = -1,
        shmctl(id: c_int, cmd: c_int, buf: *mut c_void) -> c_int = -1,
        semget(key: c_int, count: c_int, flags: c_int) -> c_int = -1,
        semop(id: c_int, ops: *mut c_void, count: usize) -> c_int = -1,
        semctl(id: c_int, num: c_int, cmd: c_int, arg: *mut c_void) -> c_int = -1,
        msgget(key: c_int, flags: c_int) -> c_int = -1,
        msgsnd(id: c_int, msg: *const c_void, size: usize, flags: c_int) -> c_int = -1,
    }

    // ------------------------------------------------------------------ backtrace

    /// Walk the frame-pointer chain and record the return addresses.
    ///
    /// The same walk the kernel does when it reports a fault, and it works for the
    /// same reason: this tree is built with frame pointers, so `x29` holds the
    /// previous frame and `x29 + 8` its return address. A frame whose pointer is not
    /// above the last one ends the walk — that is what stops a corrupted chain from
    /// becoming an infinite loop, which is exactly the situation a backtrace is
    /// usually called from.
    ///
    /// This frame's `x29`. Separated from [`backtrace`] so the one register name in
    /// the module that is not portable sits behind an architecture gate rather than
    /// inside a function that would otherwise compile anywhere.
    #[cfg(target_arch = "aarch64")]
    fn frame_pointer() -> u64 {
        let fp: u64;
        // SAFETY: reads a register; touches no memory.
        unsafe { core::arch::asm!("mov {}, x29", out(reg) fp, options(nomem, nostack)) };
        fp
    }

    /// On anything else there is no walk to do — the shipping build is AArch64, and
    /// a host build of this crate exists only so the portable half can be tested.
    #[cfg(not(target_arch = "aarch64"))]
    fn frame_pointer() -> u64 {
        0
    }

    /// # Safety
    /// C ABI: `buf` is valid for `size` pointers.
    #[no_mangle]
    pub unsafe extern "C" fn backtrace(buf: *mut *mut c_void, size: c_int) -> c_int {
        if buf.is_null() || size <= 0 {
            return 0;
        }
        let mut frame = frame_pointer();
        let mut count = 0;
        let mut previous = 0u64;
        while count < size && frame > previous && frame % 16 == 0 {
            // SAFETY: `frame` is a frame pointer in this thread's own stack; the two
            // words at it are the saved `x29` and `x30` the prologue stored.
            let (next, ret) = unsafe {
                (
                    core::ptr::read_volatile(frame as *const u64),
                    core::ptr::read_volatile((frame + 8) as *const u64),
                )
            };
            if ret == 0 {
                break;
            }
            // SAFETY: the caller's contract; `count < size`.
            unsafe { buf.add(count as usize).write(ret as *mut c_void) };
            count += 1;
            previous = frame;
            frame = next;
        }
        count
    }

    /// `backtrace_symbols`: turning an address into a name needs a symbol table in
    /// the running image, and this one is stripped. NULL is the documented failure,
    /// and a caller that gets it prints the addresses instead — which are still
    /// useful, because `scripts/symbolize.sh` resolves them against the ELF.
    ///
    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn backtrace_symbols(
        _buf: *const *mut c_void,
        _size: c_int,
    ) -> *mut *mut c_char {
        core::ptr::null_mut()
    }

    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn backtrace_symbols_fd(
        buf: *const *mut c_void,
        size: c_int,
        fd: c_int,
    ) {
        for i in 0..size.max(0) {
            // SAFETY: the caller's contract.
            let addr = unsafe { *buf.add(i as usize) } as usize;
            let mut line = [0u8; 32];
            let mut at = 0;
            for (dst, src) in line.iter_mut().zip(b"0x".iter()) {
                *dst = *src;
                at += 1;
            }
            // Hex, most significant nibble first, no leading zeroes.
            let mut started = false;
            for shift in (0..16).rev() {
                let nibble = ((addr >> (shift * 4)) & 0xf) as u8;
                if nibble != 0 || started || shift == 0 {
                    started = true;
                    line[at] = b"0123456789abcdef"[nibble as usize];
                    at += 1;
                }
            }
            line[at] = b'\n';
            at += 1;
            // SAFETY: `line[..at]` is initialised.
            unsafe { crate::file::exports::write(fd, line.as_ptr().cast(), at) };
        }
    }
}

/// `setjmp` and `longjmp`, in assembly, using glibc's `jmp_buf` layout.
///
/// These cannot be written in Rust: saving and restoring the callee-saved registers
/// *is* the operation, and a compiler is entitled to assume a function returns once.
/// The layout is glibc's because an object compiled against `<setjmp.h>` allocated
/// the buffer from glibc's `sizeof`, and the offsets below are the ones it expects:
/// `x19`–`x28` at 0, `x29` at 80, `x30` at 88, `sp` at 104, and `d8`–`d15` from 112.
/// The gap at 96 is where glibc keeps a pointer-mangling slot this library does not
/// use.
///
/// This is what C++ exception unwinding falls back to in a `-fno-exceptions` build,
/// and what a program's own error handling uses directly.
#[cfg(all(not(test), target_arch = "aarch64"))]
pub mod jump {
    /// # Safety
    /// C ABI: `buf` is at least 192 bytes, 16-byte aligned.
    #[no_mangle]
    #[unsafe(naked)]
    pub unsafe extern "C" fn setjmp(_buf: *mut u64) -> core::ffi::c_int {
        // SAFETY: writes only into the caller's buffer and returns 0.
        core::arch::naked_asm!(
            "stp x19, x20, [x0, #0]",
            "stp x21, x22, [x0, #16]",
            "stp x23, x24, [x0, #32]",
            "stp x25, x26, [x0, #48]",
            "stp x27, x28, [x0, #64]",
            "stp x29, x30, [x0, #80]",
            "mov x1, sp",
            "str x1, [x0, #104]",
            "stp d8,  d9,  [x0, #112]",
            "stp d10, d11, [x0, #128]",
            "stp d12, d13, [x0, #144]",
            "stp d14, d15, [x0, #160]",
            "mov w0, #0",
            "ret",
        )
    }

    /// # Safety
    /// C ABI: `buf` was filled by [`setjmp`] in a frame that has not returned.
    #[no_mangle]
    #[unsafe(naked)]
    pub unsafe extern "C" fn longjmp(_buf: *mut u64, _value: core::ffi::c_int) -> ! {
        // SAFETY: restores the saved frame and jumps to its return address.
        core::arch::naked_asm!(
            "ldp x19, x20, [x0, #0]",
            "ldp x21, x22, [x0, #16]",
            "ldp x23, x24, [x0, #32]",
            "ldp x25, x26, [x0, #48]",
            "ldp x27, x28, [x0, #64]",
            "ldp x29, x30, [x0, #80]",
            "ldr x2, [x0, #104]",
            "mov sp, x2",
            "ldp d8,  d9,  [x0, #112]",
            "ldp d10, d11, [x0, #128]",
            "ldp d12, d13, [x0, #144]",
            "ldp d14, d15, [x0, #160]",
            // C says a `longjmp` of 0 arrives as 1, so that the value can always be
            // told apart from `setjmp`'s own return.
            "cmp w1, #0",
            "csinc w0, w1, wzr, ne",
            "br x30",
        )
    }

    /// The names a program reaches through glibc's headers. `_setjmp` and
    /// `_longjmp` differ from the others only in not touching the signal mask, and
    /// nothing here touches it in either case; `__sigsetjmp`'s second argument says
    /// whether to save the mask, and there is none to save.
    ///
    /// # Safety
    /// As [`setjmp`].
    #[no_mangle]
    #[unsafe(naked)]
    pub unsafe extern "C" fn _setjmp(_buf: *mut u64) -> core::ffi::c_int {
        // SAFETY: tail call to `setjmp`, same signature.
        core::arch::naked_asm!("b {}", sym setjmp)
    }

    /// # Safety
    /// As [`setjmp`]; the second argument is ignored.
    #[no_mangle]
    #[unsafe(naked)]
    pub unsafe extern "C" fn __sigsetjmp(_buf: *mut u64, _save_mask: core::ffi::c_int) -> core::ffi::c_int {
        // SAFETY: tail call; the extra argument is left in x1 and unused.
        core::arch::naked_asm!("b {}", sym setjmp)
    }

    /// # Safety
    /// As [`longjmp`].
    #[no_mangle]
    #[unsafe(naked)]
    pub unsafe extern "C" fn _longjmp(_buf: *mut u64, _value: core::ffi::c_int) -> ! {
        // SAFETY: tail call to `longjmp`, same signature.
        core::arch::naked_asm!("b {}", sym longjmp)
    }

    /// # Safety
    /// As [`longjmp`].
    #[no_mangle]
    #[unsafe(naked)]
    pub unsafe extern "C" fn siglongjmp(_buf: *mut u64, _value: core::ffi::c_int) -> ! {
        // SAFETY: tail call to `longjmp`, same signature.
        core::arch::naked_asm!("b {}", sym longjmp)
    }

    /// The fortified `longjmp`.
    ///
    /// `_FORTIFY_SOURCE` emits this instead, and what it buys is a check that the
    /// stack being jumped to is *outward* — a `longjmp` into a frame that has
    /// already returned is undefined behaviour that usually manifests much later.
    /// The check is the one glibc makes: the saved `sp` must be at or above the
    /// current one, since stacks grow down.
    ///
    /// # Safety
    /// As [`longjmp`].
    #[no_mangle]
    pub unsafe extern "C" fn __longjmp_chk(buf: *mut u64, value: core::ffi::c_int) -> ! {
        let mut sp: u64;
        // SAFETY: reading this frame's own stack pointer.
        unsafe { core::arch::asm!("mov {}, sp", out(reg) sp, options(nomem, nostack)) };
        // SAFETY: the caller's contract; slot 13 is where `setjmp` put the stack
        // pointer.
        let target = unsafe { *buf.add(13) };
        if target < sp {
            crate::chk_fail("longjmp");
        }
        // SAFETY: checked to be an outward jump.
        unsafe { longjmp(buf, value) }
    }
}

#[cfg(test)]
mod tests {
    use super::{key_of, mix};

    #[test]
    fn a_name_ends_at_the_first_equals() {
        assert_eq!(key_of(b"PATH=/bin:/usr/bin"), b"PATH");
        // A value containing `=` is still one value: only the first separator counts,
        // and splitting at the last would make `A=B=C` set the variable `A=B`.
        assert_eq!(key_of(b"A=B=C"), b"A");
        assert_eq!(key_of(b"BARE"), b"BARE");
        assert_eq!(key_of(b"=VALUE"), b"");
    }

    #[test]
    fn the_mixer_does_not_get_stuck() {
        // Every generator has a fixed point somewhere; what matters is that the
        // sequence from a real seed does not repeat immediately and that zero is not
        // absorbing — a seed of zero returning zero forever would make every
        // "random" value on this system identical.
        let mut seen = std::collections::HashSet::new();
        let mut state = mix(1);
        for _ in 0..1000 {
            state = mix(state);
            assert!(seen.insert(state), "the sequence repeated within 1000 steps");
        }
        assert_ne!(mix(0), 0);
    }
}
