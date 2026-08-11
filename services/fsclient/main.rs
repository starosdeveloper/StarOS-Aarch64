//! `fsclient` — a process with no files, reading a file.
//!
//! This program is the point of the file server. It holds exactly two capabilities:
//! send on the request endpoint, receive on the reply endpoint. It has no device,
//! no archive, no framebuffer, and — the part that matters — no mapping of the
//! initramfs anywhere in its address space. Every byte it prints out of
//! `greeting.txt` crossed an address-space boundary to get here.
//!
//! What it exercises, in order:
//!
//! 1. `Stat`, so the size is known before a byte is read.
//! 2. `Open`, then `Read` **twice at different offsets**, because a server that
//!    ignores the offset and always returns the head of the file passes a
//!    single-read test perfectly.
//! 3. Three refusals it must get back: a handle that was never opened, a file that
//!    is not in the archive, and a handle used *after* closing it. The third is the
//!    one that catches a table which reuses slot numbers — without generations it
//!    would answer with whatever opened next.
//! 4. A deliberate load from `USER_INITRD_VA`. The archive is mapped there in the
//!    server's space and nowhere in ours, so this must be a translation fault and
//!    the kernel must kill this task for it. That fault is the proof the file
//!    contents above were not simply read out of memory we already had.
//!
//! Step 4 is last on purpose: it ends the process, and everything worth printing is
//! already printed by then.

#![no_std]
#![no_main]

use core::arch::{asm, naked_asm};
use core::panic::PanicInfo;

/// Where a shared buffer we create appears in our address space.
const USER_SHARED_VA: u64 = 0x5_0000_0000;

/// Where the initramfs is mapped **in the file server's** space. Nothing is mapped
/// there in ours, which is what the last step demonstrates.
const USER_INITRD_VA: u64 = 0x9_0000_0000;

// Syscall numbers — must match `staros_abi::syscall::Syscall`.
const SYS_SEND: usize = 1;
const SYS_RECV: usize = 2;
const SYS_EXIT: usize = 4;
const SYS_CREATE_SHARED: usize = 14;
const SYS_MAP_SHARED: usize = 15;
const SYS_DEBUG_WRITE: usize = 19;

/// Our capability table, as the kernel granted it.
const EP_REQUEST: u64 = 1;
const EP_REPLY: u64 = 2;

// Protocol tags — must match `services/fssrv`.
const TAG_ERROR: u64 = 0;
const TAG_OPEN: u64 = 1;
const TAG_READ: u64 = 2;
const TAG_STAT: u64 = 3;
const TAG_CLOSE: u64 = 4;
const TAG_BYE: u64 = 9;

const ERR_NO_FILE: u64 = 1;
const ERR_BAD_HANDLE: u64 = 2;

/// The file to read, and a name deliberately not in the archive.
const FILE: &str = "greeting.txt";
const MISSING: &str = "no-such-file";

/// A member of the archive that is certainly bigger than one page — the point of
/// asking for it is that the answer must not fit in the buffer.
const PROGRAM: &str = "init.elf";

/// The first read stops here, so the second one starts at a non-zero offset. Small
/// enough to land inside a short greeting, which is what makes a server that
/// ignores the offset visibly wrong: the second chunk would repeat the first.
const FIRST_CHUNK: u64 = 6;

/// A message, laid out exactly as `staros_ipc::Message`.
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

/// Entry point: linked at `USER_BASE`, entered by the kernel with a fresh stack.
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.start"]
extern "C" fn _start() -> ! {
    naked_asm!("bl {main}", "b .", main = sym main)
}

extern "C" fn main() -> ! {
    // One page of our own memory, shared with whoever we delegate it to. The
    // server allocates nothing on our behalf: the buffer is ours, the capability
    // is ours to hand over, and taking it back needs no cooperation from anyone.
    // SAFETY: `CreateShared` allocates zeroed frames and returns a handle.
    let cap = unsafe { syscall1(SYS_CREATE_SHARED, 1) };
    if cap <= 0 {
        puts("[fsclient] no shared buffer; nothing to ask with\n");
        exit();
    }
    let cap = cap as u32;
    // SAFETY: `MapShared` maps the object this handle names into our space.
    let buffer = unsafe { syscall1(SYS_MAP_SHARED, u64::from(cap)) };
    if buffer < 0 {
        puts("[fsclient] the buffer would not map\n");
        exit();
    }
    let buffer = buffer as *mut u8;

    puts("[fsclient] two endpoint capabilities and one page of my own memory - no archive, no device\n");

    // Stat first: the size comes from the server, before we have read anything.
    put_path(buffer, FILE);
    let stat = request(TAG_STAT, [FILE.len() as u64, 0, 0], cap);
    if stat.tag != TAG_STAT {
        puts("[fsclient] the server has no '");
        puts(FILE);
        puts("'\n");
        exit();
    }
    let size = stat.words[0];
    let mut line = Line::new();
    line.str("[fsclient] stat '");
    line.str(FILE);
    line.str("' over IPC: ");
    line.dec(size);
    line.str(" bytes, mode ");
    line.oct(stat.words[1]);
    line.flush();

    // Open, then read the file in two pieces from two different offsets.
    put_path(buffer, FILE);
    let open = request(TAG_OPEN, [FILE.len() as u64, 0, 0], cap);
    if open.tag != TAG_OPEN {
        puts("[fsclient] the server refused to open it\n");
        exit();
    }
    let handle = open.words[0];

    // The file's contents are assembled into one line before anything is printed.
    // Printing each chunk as it arrives would be the obvious thing and the wrong
    // one: other tasks are writing to the same console, and a file spliced across
    // three `DebugWrite`s comes out of a four-core run interleaved with somebody
    // else's line — unreadable, and worse, unassertable.
    let mut line = Line::new();
    line.str("[fsclient] read '");
    line.str(FILE);
    line.str("' through fssrv in 2 chunks: ");
    let mut read_total = 0;
    let mut offset = 0;
    let mut chunks = 0;
    loop {
        let want = if chunks == 0 { FIRST_CHUNK } else { size };
        let reply = request(TAG_READ, [handle, offset, want], cap);
        if reply.tag != TAG_READ || reply.words[0] == 0 {
            break;
        }
        let got = reply.words[0];
        // Take the bytes the server just wrote into our page — the file's own
        // contents, in a process that cannot see the file.
        // SAFETY: the server wrote `got` bytes into the page we mapped, and `got`
        // is bounded by the page it was given.
        let chunk = unsafe { core::slice::from_raw_parts(buffer as *const u8, got as usize) };
        line.bytes(chunk);
        read_total += got;
        offset += got;
        chunks += 1;
        if reply.words[1] == 0 || chunks == 2 {
            break;
        }
    }
    line.flush();

    let mut line = Line::new();
    line.str("[fsclient] ");
    line.dec(read_total);
    line.str(" of ");
    line.dec(size);
    line.str(" bytes in ");
    line.dec(chunks);
    line.str(" reads, the second one from offset ");
    line.dec(FIRST_CHUNK);
    line.flush();

    // Three things the server must refuse. Each is checked separately, because a
    // server that refuses *everything* would pass any one of them.
    let bogus = request(TAG_READ, [handle ^ 0xDEAD_0000, 0, 8], cap);
    let bad_handle = bogus.tag == TAG_ERROR && bogus.words[0] == ERR_BAD_HANDLE;

    put_path(buffer, MISSING);
    let missing = request(TAG_OPEN, [MISSING.len() as u64, 0, 0], cap);
    let no_file = missing.tag == TAG_ERROR && missing.words[0] == ERR_NO_FILE;

    let closed = request(TAG_CLOSE, [handle, 0, 0], 0);
    let reused = request(TAG_READ, [handle, 0, 8], cap);
    let after_close =
        closed.tag == TAG_CLOSE && reused.tag == TAG_ERROR && reused.words[0] == ERR_BAD_HANDLE;

    // A path length that is a lie: far more than the one page this buffer holds.
    // The server must get the size from the capability rather than from us, and
    // refuse. If it believed the number, *it* would be the process that faulted.
    put_path(buffer, FILE);
    let liar = request(TAG_STAT, [1 << 20, 0, 0], cap);
    let lie_refused = liar.tag == TAG_ERROR;

    if bad_handle && no_file && after_close && lie_refused {
        puts("[fsclient] fssrv refused an unopened handle, a missing file, a closed handle and a lied-about length\n");
    } else {
        puts("[fsclient] fssrv answered something it should have refused\n");
    }

    // And the other half of the same rule, on the read path: ask for a file far
    // larger than the buffer it must land in, in one request. The answer has to be
    // one page and the rest reported as remaining — a server that copied what was
    // asked for would run off the end of our mapping and take the fault itself.
    put_path(buffer, PROGRAM);
    let big = request(TAG_OPEN, [PROGRAM.len() as u64, 0, 0], cap);
    if big.tag == TAG_OPEN {
        let (big_handle, big_size) = (big.words[0], big.words[1]);
        let chunk = request(TAG_READ, [big_handle, 0, big_size], cap);
        let mut line = Line::new();
        line.str("[fsclient] asked for all ");
        line.dec(big_size);
        line.str(" bytes of '");
        line.str(PROGRAM);
        line.str("' into a 4096-byte buffer and got ");
        line.dec(chunk.words[0]);
        line.str(", with ");
        line.dec(chunk.words[1]);
        line.str(" left");
        line.flush();
        let _ = request(TAG_CLOSE, [big_handle, 0, 0], 0);
    }

    // Let the server finish and report; after this it stops receiving.
    let mut bye = Message::new();
    bye.tag = TAG_BYE;
    // SAFETY: `Send` reads one `Message` through this pointer.
    let _ = unsafe { syscall2(SYS_SEND, EP_REQUEST, core::ptr::addr_of!(bye) as u64) };

    // And the proof that none of the above was a shortcut through memory we already
    // had: the archive lives at this address *in the server*, and this load must
    // fault. If it ever returns, the file contents printed above prove nothing —
    // they could have come from a mapping this process was quietly given.
    puts("[fsclient] the archive is at 0x900000000 in fssrv; touching it here must fault\n");
    let stolen = prove_isolation();
    let mut line = Line::new();
    line.str("[fsclient] I READ THE ARCHIVE DIRECTLY - the file server was never needed, first byte ");
    line.dec(u64::from(stolen));
    line.flush();
    exit();
}

/// Reach for the server's archive, three calls deep.
///
/// The depth is the point. The kernel's fault report walks the `x29` chain, and a
/// chain is only worth printing if there is more than one frame in it — a fault
/// raised straight from `main` proves the handler prints *an* address, not that it
/// follows the stack. These two functions are `#[inline(never)]` because at
/// `-Copt-level=2` the compiler would otherwise flatten them into `main` and the
/// backtrace would silently shrink back to what it was.
#[inline(never)]
fn prove_isolation() -> u8 {
    touch_archive()
}

#[inline(never)]
fn touch_archive() -> u8 {
    // SAFETY: deliberately unsound. Nothing is mapped at this address in our space,
    // and the kernel's fault handler kills this task — which is the observation.
    unsafe { (USER_INITRD_VA as *const u8).read_volatile() }
}

/// Send one request and wait for its reply.
fn request(tag: u64, words: [u64; 3], cap: u32) -> Message {
    let mut msg = Message::new();
    msg.tag = tag;
    msg.words[0] = words[0];
    msg.words[1] = words[1];
    msg.words[2] = words[2];
    msg.cap = cap;
    // SAFETY: `Send` reads one `Message` through this pointer, `Recv` writes one
    // through the other; both are on our stack and both are `#[repr(C)]`.
    unsafe {
        let _ = syscall2(SYS_SEND, EP_REQUEST, core::ptr::addr_of!(msg) as u64);
        let mut reply = Message::new();
        if syscall2(SYS_RECV, EP_REPLY, core::ptr::addr_of_mut!(reply) as u64) < 0 {
            return Message::new();
        }
        reply
    }
}

/// Copy a path into the shared buffer, where the server will read it from.
fn put_path(buffer: *mut u8, path: &str) {
    // SAFETY: the buffer is a full page we mapped; paths here are far shorter.
    unsafe {
        for (i, &b) in path.as_bytes().iter().enumerate() {
            buffer.add(i).write_volatile(b);
        }
    }
}

/// Write a string to the debug console in one syscall.
fn puts(s: &str) {
    // SAFETY: `DebugWrite` reads `len` bytes from `ptr` after walking our tables.
    unsafe {
        let _ = syscall2(SYS_DEBUG_WRITE, s.as_ptr() as u64, s.len() as u64);
    }
}

/// One console line, assembled in full before any of it is written.
///
/// The console is shared with every other task in the system, and `DebugWrite` is
/// atomic per call and only per call. A line built out of several calls is a line
/// another core may write through the middle of.
struct Line {
    buf: [u8; Self::CAP],
    len: usize,
}

impl Line {
    /// Longer than any line here; anything past it is dropped rather than
    /// truncating the newline off the end.
    const CAP: usize = 256;

    const fn new() -> Self {
        Self { buf: [0; Self::CAP], len: 0 }
    }

    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    /// Append raw bytes, skipping any newline: the contents of a file end in one,
    /// and it belongs at the end of the line, not in the middle of it.
    fn bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if b == b'\n' || self.len == Self::CAP - 1 {
                continue;
            }
            self.buf[self.len] = b;
            self.len += 1;
        }
    }

    fn dec(&mut self, value: u64) {
        self.radix(value, 10);
    }

    /// File modes are only readable in octal.
    fn oct(&mut self, value: u64) {
        self.radix(value, 8);
    }

    fn radix(&mut self, mut value: u64, radix: u64) {
        let mut digits = [0u8; 24];
        let mut n = 0;
        loop {
            digits[n] = b'0' + (value % radix) as u8;
            value /= radix;
            n += 1;
            if value == 0 {
                break;
            }
        }
        for i in 0..n {
            self.bytes(&[digits[n - 1 - i]]);
        }
    }

    /// Write the whole line, newline included, in one syscall.
    fn flush(&mut self) {
        self.buf[self.len] = b'\n';
        self.len += 1;
        // SAFETY: `DebugWrite` reads `len` bytes from `ptr`, all of them inside the
        // buffer we own.
        unsafe {
            let _ = syscall2(SYS_DEBUG_WRITE, self.buf.as_ptr() as u64, self.len as u64);
        }
        self.len = 0;
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
