//! `fssrv` — the process that owns the files.
//!
//! The initramfs has been readable in user space since the device manager unpacked
//! it, but only by the one process the kernel handed the archive to. That is a
//! filesystem the way a single `include_bytes!` is a filesystem: whoever holds the
//! bytes can read them, and nobody else can read anything. This service is where
//! files stop being one process's private memory and become something any process
//! can ask for.
//!
//! It is an ordinary EL0 process. What makes it the file server is one thing the
//! kernel gave it and gave nobody else: a read-only mapping of the CPIO archive at
//! [`USER_INITRD_VA`], seeded beside its process id. Its clients hold two endpoint
//! capabilities and nothing else — no device, no archive, no mapping. The bytes
//! they end up printing crossed an address-space boundary to reach them.
//!
//! ## The protocol
//! Bulk data travels through a shared buffer the *client* allocates and delegates,
//! exactly as in `displaysrv`, and for the same reason: the pages are the client's,
//! so the server keeps no per-client bookkeeping and revoking the capability ends
//! the conversation with nothing to clean up.
//!
//! ```text
//! tag = 1 Open   words[0] = path length,  cap = buffer holding the path
//!                -> words[0] = handle, words[1] = size in bytes
//! tag = 2 Read   words[0] = handle, words[1] = offset, words[2] = max bytes,
//!                cap = buffer to fill
//!                -> words[0] = bytes written, words[1] = bytes left after them
//! tag = 3 Stat   words[0] = path length,  cap = buffer holding the path
//!                -> words[0] = size, words[1] = mode bits
//! tag = 4 Close  words[0] = handle
//! tag = 9 Bye    the last client is done; the server may exit
//! ```
//!
//! A refusal is `tag = 0` with an [`ERR_*`](ERR_NO_FILE) code in `words[0]`, never
//! a silent zero-length read: "the file is empty" and "there is no such file" are
//! different answers, and a client that cannot tell them apart will one day ship a
//! blank screen instead of an error.
//!
//! ## What the server does not believe
//! Every length in a request is a number a client chose. The path length is clamped
//! to the buffer the capability actually names ([`SYS_SHARED_PAGES`], not the
//! sender's word for it) and to [`MAX_PATH`]; the read length is clamped the same
//! way and to what is left of the file. Believing an inflated length would make the
//! *server* run off the end of a mapping and take the fault — the wrong process
//! punished for someone else's arithmetic.
//!
//! Writing is deliberately absent. A read-only initramfs is the whole of what the
//! GUI phases need (fonts, `.qml`, translations); a real filesystem with a journal
//! is a later, separate piece of work and pretending otherwise here would mean
//! shipping a write path nothing exercises.

#![no_std]
#![no_main]

use core::arch::{asm, naked_asm};
use core::panic::PanicInfo;

use staros_abi::fsproto::{
    ERR_BAD_HANDLE, ERR_MALFORMED, ERR_NO_FILE, MAX_PATH, TAG_BYE, TAG_CLOSE, TAG_ERROR, TAG_LIST,
    TAG_OPEN, TAG_READ, TAG_STAT,
};
use staros_cpio::{Archive, Entry};

/// The kernel-seeded data page. Process id at `+0`; the initramfs's user address
/// at `+24` and its length at `+32` (see `AddressSpace::write_initrd_info`).
const USER_DATA_VA: u64 = 0x4_0000_0000;

// Syscall numbers — must match `staros_abi::syscall::Syscall`.
const SYS_SEND: usize = 1;
const SYS_RECV: usize = 2;
const SYS_EXIT: usize = 4;
const SYS_MAP_SHARED: usize = 15;
const SYS_DEBUG_WRITE: usize = 19;
const SYS_SHARED_PAGES: usize = 28;

/// Our capability table, as the kernel granted it: receive on the request
/// endpoint, send on the reply endpoint.
const EP_REQUEST: u64 = 1;
const EP_REPLY: u64 = 2;

/// How many files may be open at once. Small on purpose: the table is fixed, so a
/// client cannot make the server allocate.
const MAX_OPEN: usize = 8;

/// A rubbish message must not become an infinite loop, and neither must a client
/// that never says goodbye: `Recv` returns immediately whenever a sender is
/// queued, so an unbounded server spins as fast as the endpoint can feed it.
/// 2048, raised from 512 the day a real application became a client: Qt reads a
/// typeface whole, the reads arrive through a 4 KiB bounce buffer, and four faces of
/// one family are already several hundred round trips before a window is painted. A
/// bound that a legitimate client crosses is not a safety net — it is a server that
/// stops answering mid-conversation, and the symptom lands on the *client*, blocked
/// for ever on a reply from a server that has already printed its tally and exited.
const MAX_REQUESTS: u32 = 2048;
/// How many malformed messages to answer before concluding the client is broken.
const MAX_REJECTED: u32 = 8;

/// Bytes in a page — the unit [`SYS_SHARED_PAGES`] answers in.
const PAGE: usize = 4096;

/// A message, laid out exactly as `staros_ipc::Message`: tag, four words, then the
/// capability handle.
#[repr(C)]
struct Message {
    tag: u64,
    words: [u64; 4],
    cap: u32,
}

impl Message {
    const fn new() -> Self {
        Self { tag: 0, words: [0; 4], cap: 0 }
    }
}

/// One open file: the archive member it names, and the generation that was current
/// when it was opened.
#[derive(Clone, Copy)]
struct Open {
    entry: Entry<'static>,
    generation: u32,
}

/// The open-file table.
///
/// A handle is `(generation << 8) | (slot + 1)`, so slot 0 is never handle 0 and a
/// handle from a closed file does not silently become a handle to whatever opened
/// next in the same slot. Without the generation the table would hand out the same
/// number twice and answer both — a client's use-after-close would then read
/// another client's file and look, from every log line, like it worked.
struct Table {
    slots: [Option<Open>; MAX_OPEN],
    next_generation: u32,
}

impl Table {
    const fn new() -> Self {
        Self { slots: [None; MAX_OPEN], next_generation: 1 }
    }

    /// Take a free slot for `entry` and return its handle, or `None` if full.
    fn insert(&mut self, entry: Entry<'static>) -> Option<u64> {
        let slot = self.slots.iter().position(Option::is_none)?;
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        self.slots[slot] = Some(Open { entry, generation });
        Some((u64::from(generation) << 8) | (slot as u64 + 1))
    }

    /// The file a handle names, or `None` if it names nothing open.
    fn get(&self, handle: u64) -> Option<Entry<'static>> {
        let slot = (handle & 0xFF).checked_sub(1)? as usize;
        let open = self.slots.get(slot)?.as_ref()?;
        (u64::from(open.generation) == handle >> 8).then_some(open.entry)
    }

    /// Close a handle. Returns whether it named anything.
    fn remove(&mut self, handle: u64) -> bool {
        if self.get(handle).is_none() {
            return false;
        }
        let slot = (handle & 0xFF) as usize - 1;
        self.slots[slot] = None;
        true
    }
}

/// Entry point: linked at `USER_BASE`, entered by the kernel with a fresh stack.
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.start"]
extern "C" fn _start() -> ! {
    naked_asm!("bl {main}", "b .", main = sym main)
}

extern "C" fn main() -> ! {
    // SAFETY: the kernel seeded the initramfs VA at +24 and its length at +32
    // before our TTBR0 ever ran, and mapped `[va, va + len)` read-only.
    let (initrd_va, initrd_len) = unsafe {
        (
            ((USER_DATA_VA + 24) as *const u64).read_volatile(),
            ((USER_DATA_VA + 32) as *const u32).read_volatile() as usize,
        )
    };

    // No archive is not a reason to refuse to run: the clients are already waiting
    // for replies, and a server that exits leaves them blocked forever. It serves
    // an empty archive instead, which answers every open with "no such file" —
    // the truth, and an answer.
    let archive = if initrd_len == 0 {
        puts("[fssrv] the kernel gave me no initramfs; every open will be refused\n");
        Archive::new(&[])
    } else {
        // SAFETY: the mapping above backs this slice for as long as we run, and the
        // parser never reads past `initrd_len`.
        Archive::new(unsafe { core::slice::from_raw_parts(initrd_va as *const u8, initrd_len) })
    };

    let count = archive.entries().count();
    let mut line = Line::new();
    line.put(b"[fssrv] the files are mine: ");
    line.num(count as u64);
    line.put(b" of them, served over IPC to processes that hold no archive\n");
    line.flush();

    let mut table = Table::new();
    let mut served: u32 = 0;
    let mut refused: u32 = 0;
    let mut bytes_out = 0u64;
    let mut rejected: u32 = 0;

    while served < MAX_REQUESTS && rejected < MAX_REJECTED {
        let mut msg = Message::new();
        // SAFETY: `Recv` writes one `Message` through this pointer and blocks until
        // a client sends one.
        let rc = unsafe { syscall2(SYS_RECV, EP_REQUEST, core::ptr::addr_of_mut!(msg) as u64) };
        if rc < 0 {
            puts("[fssrv] receive failed\n");
            break;
        }
        if msg.tag == TAG_BYE {
            break;
        }

        let reply = serve(&archive, &mut table, &msg, &mut bytes_out);
        if reply.tag == TAG_ERROR {
            refused += 1;
            if reply.words[0] == ERR_MALFORMED {
                rejected += 1;
            }
        }
        served += 1;

        // SAFETY: `Send` reads one `Message` through this pointer.
        let sent = unsafe { syscall2(SYS_SEND, EP_REPLY, core::ptr::addr_of!(reply) as u64) };
        if sent < 0 {
            puts("[fssrv] reply failed; the client is gone\n");
            break;
        }
    }

    let mut line = Line::new();
    line.put(b"[fssrv] served ");
    line.num(u64::from(served));
    line.put(b" requests, ");
    line.num(bytes_out);
    line.put(b" bytes of file data, and refused ");
    line.num(u64::from(refused));
    line.put(b" - the archive never left this address space\n");
    line.flush();
    exit();
}

/// Answer one request. Every path through here produces a reply: a client blocked
/// in `Recv` is owed one even when its request was nonsense.
fn serve(
    archive: &Archive<'static>,
    table: &mut Table,
    msg: &Message,
    bytes_out: &mut u64,
) -> Message {
    match msg.tag {
        TAG_OPEN | TAG_STAT => {
            let Some(buffer) = Buffer::map(msg.cap) else {
                return error(ERR_MALFORMED);
            };
            let Some(path) = buffer.path(msg.words[0] as usize) else {
                return error(ERR_MALFORMED);
            };
            let Some(entry) = archive.find(path).filter(Entry::is_file) else {
                return error(ERR_NO_FILE);
            };
            let mut reply = Message::new();
            reply.tag = msg.tag;
            if msg.tag == TAG_STAT {
                reply.words[0] = entry.data.len() as u64;
                reply.words[1] = u64::from(entry.mode);
                return reply;
            }
            match table.insert(entry) {
                Some(handle) => {
                    reply.words[0] = handle;
                    reply.words[1] = entry.data.len() as u64;
                    reply
                }
                // The table is fixed-size, so "full" is a real answer rather than a
                // reason to grow: a client that never closes gets refused, and the
                // other clients keep being served.
                None => error(ERR_BAD_HANDLE),
            }
        }

        TAG_READ => {
            let Some(entry) = table.get(msg.words[0]) else {
                return error(ERR_BAD_HANDLE);
            };
            let Some(buffer) = Buffer::map(msg.cap) else {
                return error(ERR_MALFORMED);
            };
            // Past the end is not an error — it is end of file, which is what a
            // reader loops until. Only the *offset* arithmetic has to be careful:
            // an offset beyond the file must produce zero bytes, not a wrapped
            // length.
            let offset = msg.words[1].min(entry.data.len() as u64) as usize;
            let available = entry.data.len() - offset;
            let len = (msg.words[2] as usize).min(available).min(buffer.len);
            // SAFETY: `len` is bounded by both the file's remaining bytes and the
            // pages the capability actually names, and the buffer is mapped
            // read/write into our space.
            unsafe {
                let dst = buffer.base;
                for i in 0..len {
                    dst.add(i).write_volatile(entry.data[offset + i]);
                }
            }
            *bytes_out += len as u64;
            let mut reply = Message::new();
            reply.tag = TAG_READ;
            reply.words[0] = len as u64;
            reply.words[1] = (available - len) as u64;
            reply
        }

        TAG_CLOSE => {
            if !table.remove(msg.words[0]) {
                return error(ERR_BAD_HANDLE);
            }
            let mut reply = Message::new();
            reply.tag = TAG_CLOSE;
            reply
        }

        TAG_LIST => {
            let Some(buffer) = Buffer::map(msg.cap) else {
                return error(ERR_MALFORMED);
            };
            // Past the end is `ERR_NO_FILE` rather than an empty name: the client
            // stops on the refusal, and an empty name would be indistinguishable
            // from a member the archive really does hold under an empty name.
            let Some(entry) = archive.entries().nth(msg.words[0] as usize) else {
                return error(ERR_NO_FILE);
            };
            let name = entry.name.as_bytes();
            let len = name.len().min(MAX_PATH).min(buffer.len);
            // SAFETY: `len` is bounded by the pages the capability names, and the
            // buffer is mapped read/write into this address space.
            unsafe {
                for i in 0..len {
                    buffer.base.add(i).write_volatile(name[i]);
                }
            }
            let mut reply = Message::new();
            reply.tag = TAG_LIST;
            reply.words[0] = len as u64;
            reply.words[1] = entry.data.len() as u64;
            reply.words[2] = u64::from(entry.mode);
            reply
        }

        _ => error(ERR_MALFORMED),
    }
}

/// A refusal, with the reason in the first word.
fn error(code: u64) -> Message {
    let mut reply = Message::new();
    reply.tag = TAG_ERROR;
    reply.words[0] = code;
    reply
}

/// A client's shared buffer, mapped into this process, and the number of bytes the
/// *capability* says it holds — not the number the message claimed.
struct Buffer {
    base: *mut u8,
    len: usize,
}

impl Buffer {
    /// Map the buffer a delegated handle names, or `None` if the handle names no
    /// shared memory.
    ///
    /// The address comes from the kernel and is used as returned, never assumed.
    /// One object keeps one placement for the life of the task, so a client's
    /// buffer stays where it first landed and a *second* client's gets its own
    /// address — which is why this may not cache the pointer across requests, and
    /// why `len` comes from this handle's own page count rather than from the
    /// previous request's.
    fn map(cap: u32) -> Option<Self> {
        if cap == 0 {
            return None;
        }
        // SAFETY: `SharedPages` and `MapShared` only read the capability table;
        // both return a negative error rather than acting on a bad handle.
        let (pages, va) = unsafe {
            (
                syscall1(SYS_SHARED_PAGES, u64::from(cap)),
                syscall1(SYS_MAP_SHARED, u64::from(cap)),
            )
        };
        if pages <= 0 || va < 0 {
            return None;
        }
        Some(Self { base: va as *mut u8, len: pages as usize * PAGE })
    }

    /// The path at the start of the buffer, `len` bytes long as the client claims —
    /// clamped to the mapping and to [`MAX_PATH`], and rejected unless it is UTF-8.
    fn path(&self, len: usize) -> Option<&str> {
        if len == 0 || len > MAX_PATH || len > self.len {
            return None;
        }
        // SAFETY: `len` is inside the mapping the capability names, and the pages
        // are readable for as long as we hold the delegated handle.
        let bytes = unsafe { core::slice::from_raw_parts(self.base as *const u8, len) };
        core::str::from_utf8(bytes).ok()
    }
}

/// Write a string to the debug console in one syscall.
fn puts(s: &str) {
    // SAFETY: `DebugWrite` reads `len` bytes from `ptr` after walking our tables.
    unsafe {
        let _ = syscall2(SYS_DEBUG_WRITE, s.as_ptr() as u64, s.len() as u64);
    }
}

/// A line being built for `DebugWrite`, bounded and self-truncating.
///
/// This server used to print its two reports as alternating `puts` and `put_dec`
/// calls, which is one `DebugWrite` per fragment and one opening per fragment for
/// another task to write into. It showed: the boot line came out of a real run
/// split across two lines, with a display-server line wedged into the middle of it.
/// Nothing was wrong with the server and nothing was wrong with the console — the
/// line was simply never a line, and a report that is *usually* whole is the worst
/// kind of evidence, because the run where it tears is the run being read.
///
/// Truncation is what happens on overflow. A diagnostic that panics is worse than a
/// diagnostic that is short.
struct Line {
    buf: [u8; 192],
    n: usize,
}

impl Line {
    const fn new() -> Self {
        Self { buf: [0; 192], n: 0 }
    }

    fn put(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.n < self.buf.len() {
                self.buf[self.n] = b;
                self.n += 1;
            }
        }
    }

    /// Append `value` in decimal. Nothing is appended if it will not fit, which is
    /// the honest failure for a diagnostic: a truncated number reads as a real one.
    fn num(&mut self, value: u64) {
        let mut digits = [0u8; 20];
        let mut count = 0;
        let mut left = value;
        loop {
            digits[count] = b'0' + (left % 10) as u8;
            count += 1;
            left /= 10;
            if left == 0 {
                break;
            }
        }
        if count > self.buf.len() - self.n {
            return;
        }
        for i in 0..count {
            self.buf[self.n + i] = digits[count - 1 - i];
        }
        self.n += count;
    }

    /// One syscall, one line.
    fn flush(&mut self) {
        // SAFETY: `DebugWrite` reads `n` bytes from a buffer we own.
        unsafe {
            let _ = syscall2(SYS_DEBUG_WRITE, self.buf.as_ptr() as u64, self.n as u64);
        }
        self.n = 0;
    }
}

/// End this process.
fn exit() -> ! {
    // SAFETY: `Exit` never returns.
    unsafe {
        syscall1(SYS_EXIT, 0);
    }
    loop {
        core::hint::spin_loop();
    }
}

/// A one-argument syscall: number in x8, argument in x0, result in x0.
///
/// # Safety
/// The number and argument must name a syscall this process may make; the kernel
/// validates every pointer it is given against our own page tables.
unsafe fn syscall1(number: usize, a0: u64) -> isize {
    let ret;
    // SAFETY: the kernel's SVC handler preserves every register but x0.
    unsafe {
        asm!("svc #0", in("x8") number, inout("x0") a0 => ret, options(nostack));
    }
    ret
}

/// A two-argument syscall: number in x8, args in x0/x1, result in x0.
///
/// # Safety
/// As [`syscall1`].
unsafe fn syscall2(number: usize, a0: u64, a1: u64) -> isize {
    let ret;
    // SAFETY: as `syscall1`, with a second argument in x1.
    unsafe {
        asm!("svc #0", in("x8") number, inout("x0") a0 => ret, in("x1") a1, options(nostack));
    }
    ret
}

/// Nothing here panics deliberately; the lang item has to exist regardless.
#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    exit()
}
