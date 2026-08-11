//! `displaysrv` — the process that owns the screen.
//!
//! Until now the framebuffer belonged to the kernel, which mirrored its log onto
//! it. That was right for bringing a panel up and wrong for everything after: while
//! the kernel owns the pixels, no program can own a window. This service is where
//! the screen goes.
//!
//! It is an ordinary EL0 process. What makes it the display server is one thing the
//! kernel gave it and gave nobody else: a mapping of the pixel buffer, at
//! [`FB_VA`], with its geometry seeded beside the process id. Every other process
//! reaches the screen only by asking this one.
//!
//! ## The protocol, and why it is this small
//! A client allocates its own pixels with `CreateShared`, draws into them, and
//! sends one message naming the buffer and where it goes:
//!
//! ```text
//! tag = 1 (Commit)
//! words[0] = width, words[1] = height   in pixels
//! words[2] = x,     words[3] = y        top-left corner on screen
//! cap      = the shared buffer, width * height * 4 bytes of xRGB8888
//! ```
//!
//! The server maps it, copies the rectangle onto the screen, and replies. The
//! *client* allocating the buffer rather than the server is deliberate: the pages
//! are the client's, the capability is the client's to delegate, and revoking it
//! takes the surface away with no bookkeeping here. A server that handed out
//! buffers would have to track who still holds them, which is the sort of thing
//! capabilities exist to avoid.
//!
//! Damage tracking, several surfaces, z-order and vsync are all absent on purpose:
//! this is the phase that moves the screen out of the kernel, not the phase that
//! composites well. What it must prove is that a process with no privileges beyond
//! a delegated capability can put pixels on a display it does not own.

#![no_std]
#![no_main]

use core::arch::{asm, naked_asm};
use core::panic::PanicInfo;

/// The kernel-seeded data page. Process id at `+0`; the framebuffer's user address
/// at `+40`, width at `+48`, height at `+52`, stride at `+56` (see
/// `AddressSpace::write_fb_info`).
const USER_DATA_VA: u64 = 0x4_0000_0000;

/// Where a mapped shared buffer appears in our address space.
const USER_SHARED_VA: u64 = 0x5_0000_0000;

// Syscall numbers — must match `staros_abi::syscall::Syscall`.
const SYS_RECV: usize = 2;
const SYS_SEND: usize = 1;
const SYS_EXIT: usize = 4;
const SYS_DEBUG_WRITE: usize = 19;
const SYS_MAP_SHARED: usize = 15;

/// Our capability table, as the kernel granted it: receive on the client endpoint,
/// reply on the other.
const EP_REQUEST: u64 = 1;
const EP_REPLY: u64 = 2;

/// How many commits to serve before reporting and exiting. The demo has one client;
/// a real server would loop forever, and would need `WaitAny` to do anything else
/// while it waited.
const COMMITS_EXPECTED: u32 = 1;

/// Bytes per pixel. xRGB8888 throughout — what `ramfb` gives us and what the
/// kernel's own console assumes.
const BPP: usize = 4;

/// The background this server paints before serving anyone, so a screenshot can
/// tell "the display server is running" from "the kernel stopped drawing".
const BACKGROUND: u32 = 0x0010_2030;

/// A message, laid out exactly as `staros_ipc::Message`: tag, `MESSAGE_WORDS`
/// words, then the capability handle. Four words, not six — getting that wrong
/// puts `cap` sixteen bytes past where the kernel writes it, and the symptom is a
/// server that receives real messages and calls every one of them malformed.
#[repr(C)]
struct Message {
    tag: u64,
    words: [u64; 4],
    cap: u32,
}

impl Message {
    const fn new() -> Self {
        Self {
            tag: 0,
            words: [0; 4],
            cap: 0,
        }
    }
}

/// The framebuffer as this process sees it.
struct Screen {
    base: *mut u8,
    width: usize,
    height: usize,
    stride: usize,
}

impl Screen {
    /// Read the geometry the kernel seeded, or `None` if it gave us no screen.
    fn from_seed() -> Option<Self> {
        // SAFETY: the kernel wrote these four fields into our data page before our
        // TTBR0 ever ran, and mapped the pixels at the address in the first.
        unsafe {
            let base = ((USER_DATA_VA + 40) as *const u64).read_volatile();
            let width = ((USER_DATA_VA + 48) as *const u32).read_volatile() as usize;
            let height = ((USER_DATA_VA + 52) as *const u32).read_volatile() as usize;
            let stride = ((USER_DATA_VA + 56) as *const u32).read_volatile() as usize;
            if base == 0 || width == 0 || height == 0 || stride < width * BPP {
                return None;
            }
            Some(Self {
                base: base as *mut u8,
                width,
                height,
                stride,
            })
        }
    }

    /// Fill the whole screen with one colour.
    fn clear(&mut self, colour: u32) {
        for y in 0..self.height {
            for x in 0..self.width {
                self.put(x, y, colour);
            }
        }
    }

    /// Write one pixel, clipped. Clipping rather than trusting: the geometry comes
    /// from firmware, and a stride that does not match the width is the normal
    /// case, not the exception.
    fn put(&mut self, x: usize, y: usize, colour: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let offset = y * self.stride + x * BPP;
        // SAFETY: `offset` is inside `height * stride`, which is what the kernel
        // mapped; the buffer is ours alone while we run.
        unsafe {
            self.base.add(offset).cast::<u32>().write_volatile(colour);
        }
    }

    /// Copy a client's `width * height` rectangle of pixels to `(x, y)`, and return
    /// how many pixels actually landed on screen.
    fn blit(&mut self, pixels: *const u32, w: usize, h: usize, x: usize, y: usize) -> usize {
        let mut drawn = 0;
        for row in 0..h {
            for col in 0..w {
                let (sx, sy) = (x + col, y + row);
                if sx >= self.width || sy >= self.height {
                    continue;
                }
                // SAFETY: the buffer the client shared is `w * h` pixels; the
                // capability it delegated is what makes it readable here.
                let colour = unsafe { pixels.add(row * w + col).read_volatile() };
                self.put(sx, sy, colour);
                drawn += 1;
            }
        }
        drawn
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
    let Some(mut screen) = Screen::from_seed() else {
        puts("[displaysrv] the kernel gave me no screen; nothing to serve\n");
        exit();
    };
    screen.clear(BACKGROUND);
    puts("[displaysrv] the screen is mine: kernel output stopped, pixels are a process's now\n");

    let mut served = 0;
    let mut pixels_drawn = 0;
    // A malformed message must not become an infinite loop: `Recv` returns at once
    // when a sender is queued, so a server that only `continue`s on rubbish spins
    // as fast as the endpoint can feed it. Bound it.
    let mut rejected = 0;
    while served < COMMITS_EXPECTED && rejected < 4 {
        let mut msg = Message::new();
        // SAFETY: `Recv` writes one `Message` through this pointer and blocks until
        // a client sends one.
        let rc = unsafe { syscall2(SYS_RECV, EP_REQUEST, core::ptr::addr_of_mut!(msg) as u64) };
        if rc < 0 {
            puts("[displaysrv] receive failed\n");
            break;
        }
        if msg.tag != 1 || msg.cap == 0 {
            puts("[displaysrv] a message that is not a commit; ignoring it\n");
            rejected += 1;
            continue;
        }
        // Map the client's buffer. The capability it delegated is the whole
        // permission: we can reach these pixels and no others.
        // SAFETY: `MapShared` maps the object the handle names and returns its VA.
        let va = unsafe { syscall1(SYS_MAP_SHARED, u64::from(msg.cap)) };
        if va < 0 {
            puts("[displaysrv] the client's buffer would not map\n");
            rejected += 1;
            continue;
        }
        let (w, h) = (msg.words[0] as usize, msg.words[1] as usize);
        let (x, y) = (msg.words[2] as usize, msg.words[3] as usize);
        pixels_drawn += screen.blit(USER_SHARED_VA as *const u32, w, h, x, y);
        served += 1;

        // Reply, so the client knows its pixels are on the glass rather than
        // merely sent. Same message back, with the count.
        let mut reply = Message::new();
        reply.tag = 2;
        reply.words[0] = pixels_drawn as u64;
        // SAFETY: `Send` reads one `Message` through this pointer.
        let _ = unsafe { syscall2(SYS_SEND, EP_REPLY, core::ptr::addr_of!(reply) as u64) };
    }

    puts("[displaysrv] composited a client surface onto a screen the client cannot touch\n");
    exit();
}

/// Write a string to the debug console in one syscall.
fn puts(s: &str) {
    // SAFETY: `DebugWrite` reads `len` bytes from `ptr` after walking our tables.
    unsafe {
        let _ = syscall2(SYS_DEBUG_WRITE, s.as_ptr() as u64, s.len() as u64);
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
