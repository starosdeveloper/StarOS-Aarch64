//! `displaysrv` — the process that owns the screen.
//!
//! Until now the framebuffer belonged to the kernel, which mirrored its log onto
//! it. That was right for bringing a panel up and wrong for everything after: while
//! the kernel owns the pixels, no program can own a window. This service is where
//! the screen goes.
//!
//! It is an ordinary EL0 process. What makes it the display server is one thing the
//! kernel gave it and gave nobody else: a mapping of the pixel buffer, at the
//! address seeded beside its process id, with its geometry. Every other process
//! reaches the screen only by asking this one.
//!
//! ## The protocol
//!
//! ```text
//! tag = 6 Screen   no arguments
//!                  -> words[0] = width, words[1] = height in pixels,
//!                     words[2] = bits per pixel, words[3] = 1 for xRGB8888
//! tag = 1 Create   words[0] = width, words[1] = height   in pixels
//!                  words[2] = x,     words[3] = y        top-left corner on screen
//!                  cap      = the pixel buffer, width * height * 4 bytes of xRGB8888
//!                  -> words[0] = surface id
//! tag = 3 Commit   words[0] = surface id
//!                  words[1] = damage x | y << 32     in surface-local pixels
//!                  words[2] = damage w | h << 32
//!                  -> words[0] = pixels written to the screen
//! tag = 4 Raise    words[0] = surface id              to the top of the stack
//! tag = 5 Destroy  words[0] = surface id
//! tag = 9 Bye      the last client is done; the server may report and exit
//! ```
//!
//! A reply is `tag = 2` with its result in `words[0]`, or `tag = 0` with a reason in
//! `words[0]` for a refusal. "Refused" and "did nothing" are different answers, and
//! a client that cannot tell them apart will one day show a blank window and call it
//! a slow frame.
//!
//! `Screen` comes first in that list because it has to come first in time: a client
//! cannot size a buffer before it knows what it is drawing onto, and the geometry is
//! the one thing here that no client can work out for itself. The stride is
//! deliberately *not* reported — it is this server's business, the client's pixels
//! are always packed, and a client that knew the stride would eventually assume its
//! own buffer had one.
//!
//! ## Three decisions, and what each one costs
//!
//! **The client allocates its own pixels.** It creates the buffer with
//! `CreateShared`, draws into it, and delegates the capability with `Create`. The
//! pages are the client's, and revoking the capability takes the surface away with
//! no bookkeeping here. A server that handed out buffers would have to track who
//! still held them, which is the sort of thing capabilities exist to avoid.
//!
//! **Damage is a parameter, not a guess.** `Commit` names the rectangle that
//! changed, and only that rectangle is recomposited. A full-frame copy at 1080p is
//! 8 MB per frame, which is the difference between sixty frames a second and a slide
//! show — so the honest rectangle is in the protocol from the first version rather
//! than added when it is too late to change every caller.
//!
//! **Stacking is per pixel, painter's order.** Recompositing a rectangle walks every
//! surface that overlaps it, bottom to top, and the last one wins. That is slower
//! than tracking which surface owns each region and it is correct with overlap,
//! which the region approach is only after it is finished. There is no alpha: a
//! pixel belongs to exactly one surface.
//!
//! Vsync and double buffering are still absent, deliberately. This is the phase that
//! gives a window system several windows, not the phase that makes them smooth.

#![no_std]
#![no_main]

use core::arch::{asm, naked_asm};
use core::panic::PanicInfo;

/// The kernel-seeded data page. Process id at `+0`; the framebuffer's user address
/// at `+40`, width at `+48`, height at `+52`, stride at `+56` (see
/// `AddressSpace::write_fb_info`).
const USER_DATA_VA: u64 = 0x4_0000_0000;

// Syscall numbers — must match `staros_abi::syscall::Syscall`.
const SYS_SEND: usize = 1;
const SYS_RECV: usize = 2;
const SYS_EXIT: usize = 4;
const SYS_MAP_SHARED: usize = 15;
const SYS_DEBUG_WRITE: usize = 19;
const SYS_SHARED_PAGES: usize = 28;

/// Our capability table, as the kernel granted it: receive on the client endpoint,
/// reply on the other.
const EP_REQUEST: u64 = 1;
const EP_REPLY: u64 = 2;

// Request tags.
const TAG_CREATE: u64 = 1;
const TAG_COMMIT: u64 = 3;
const TAG_RAISE: u64 = 4;
const TAG_DESTROY: u64 = 5;
const TAG_SCREEN: u64 = 6;
const TAG_BYE: u64 = 9;

/// The only pixel format this server composites. Named in the reply so a client
/// gets told rather than assuming, and so the day a second format exists the old
/// clients are the ones that keep working.
const FORMAT_XRGB8888: u64 = 1;

// Reply tags.
const TAG_ERROR: u64 = 0;
const TAG_OK: u64 = 2;

// Refusal reasons.
/// The request itself is wrong: unknown tag, no buffer, zero geometry.
const ERR_MALFORMED: u64 = 1;
/// No surface by that id — never created, or already destroyed.
const ERR_NO_SURFACE: u64 = 2;
/// The buffer is smaller than the geometry claims, or would not map.
const ERR_BUFFER: u64 = 3;
/// No room for another surface.
const ERR_FULL: u64 = 4;

/// Bytes per pixel. xRGB8888 throughout — what `ramfb` gives us and what the
/// kernel's own console assumes.
const BPP: usize = 4;

/// Bytes in a page, for turning a buffer's page count into a bound.
const PAGE: usize = 4096;

/// How many surfaces may exist at once. The kernel places at most
/// `MAX_SHARED_MAPPINGS` shared buffers in one address space, and a surface is one
/// buffer, so asking for more than that would fail at the mapping rather than here
/// — worse, it would fail *after* the client had drawn a frame.
const MAX_SURFACES: usize = 8;

/// The background this server paints before serving anyone, so a screenshot can
/// tell "the display server is running" from "the kernel stopped drawing".
const BACKGROUND: u32 = 0x0010_2030;

/// How many clients say goodbye before the server reports and exits. A real server
/// runs until the machine stops; this one has to end so the demo can print its
/// tallies, and ending on a message rather than a commit count is what makes it a
/// server loop instead of a script.
const CLIENTS_EXPECTED: u32 = 1;

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
        Self { tag: 0, words: [0; 4], cap: 0 }
    }
}

/// One client surface: where its pixels are in *our* space, how big it is, and
/// where it sits on screen.
#[derive(Clone, Copy)]
struct Surface {
    id: u64,
    pixels: *const u32,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
}

impl Surface {
    /// The colour at a screen position, if this surface covers it.
    fn at(&self, sx: usize, sy: usize) -> Option<u32> {
        if sx < self.x || sy < self.y {
            return None;
        }
        let (col, row) = (sx - self.x, sy - self.y);
        if col >= self.width || row >= self.height {
            return None;
        }
        // SAFETY: `col < width` and `row < height`, and the buffer was checked at
        // `Create` to hold `width * height` pixels. The client may write to it
        // while we read — the worst that does is tear a frame, which is what a
        // shared buffer without a fence means and is the client's to fix by not
        // drawing into a surface it has committed.
        Some(unsafe { self.pixels.add(row * self.width + col).read_volatile() })
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
            Some(Self { base: base as *mut u8, width, height, stride })
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
}

/// A rectangle in screen coordinates.
#[derive(Clone, Copy)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl Rect {
    /// This rectangle cut down to the screen, or `None` if none of it is on it.
    fn clip(self, screen: &Screen) -> Option<Rect> {
        if self.x >= screen.width || self.y >= screen.height || self.w == 0 || self.h == 0 {
            return None;
        }
        Some(Rect {
            x: self.x,
            y: self.y,
            w: self.w.min(screen.width - self.x),
            h: self.h.min(screen.height - self.y),
        })
    }
}

/// Every surface, bottom of the stack first.
///
/// One array in stacking order rather than an array plus an order list: the order
/// *is* the data, and keeping it in a second place is how a raise comes to move a
/// window in the list and not on the screen.
struct Compositor {
    surfaces: [Surface; MAX_SURFACES],
    count: usize,
    next_id: u64,
}

impl Compositor {
    const fn new() -> Self {
        const EMPTY: Surface = Surface {
            id: 0,
            pixels: core::ptr::null(),
            width: 0,
            height: 0,
            x: 0,
            y: 0,
        };
        Self { surfaces: [EMPTY; MAX_SURFACES], count: 0, next_id: 1 }
    }

    /// The index of the surface with this id, if it is live.
    fn index_of(&self, id: u64) -> Option<usize> {
        self.surfaces[..self.count].iter().position(|s| s.id == id)
    }

    /// Add a surface at the top of the stack and return its id.
    fn add(&mut self, pixels: *const u32, width: usize, height: usize, x: usize, y: usize)
        -> Option<u64>
    {
        if self.count == MAX_SURFACES {
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.surfaces[self.count] = Surface { id, pixels, width, height, x, y };
        self.count += 1;
        Some(id)
    }

    /// Move a surface to the top, keeping everything below it in order.
    fn raise(&mut self, index: usize) {
        let surface = self.surfaces[index];
        self.surfaces.copy_within(index + 1..self.count, index);
        self.surfaces[self.count - 1] = surface;
    }

    /// Remove a surface, closing the gap so the stack stays contiguous.
    fn remove(&mut self, index: usize) {
        self.surfaces.copy_within(index + 1..self.count, index);
        self.count -= 1;
    }

    /// Repaint one rectangle of the screen from every surface that overlaps it,
    /// bottom to top, and return how many pixels were written.
    fn composite(&self, screen: &mut Screen, rect: Rect) -> usize {
        let Some(rect) = rect.clip(screen) else {
            return 0;
        };
        let mut painted = 0;
        for sy in rect.y..rect.y + rect.h {
            for sx in rect.x..rect.x + rect.w {
                // Painter's order: start from what the server owns, then let each
                // surface above overwrite it. The background has to be repainted
                // here and not only at start-up, or a window that moves leaves its
                // old pixels behind for ever.
                let mut colour = BACKGROUND;
                for surface in &self.surfaces[..self.count] {
                    if let Some(pixel) = surface.at(sx, sy) {
                        colour = pixel;
                    }
                }
                screen.put(sx, sy, colour);
                painted += 1;
            }
        }
        painted
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
    let mut compositor = Compositor::new();
    // The whole screen, once, so a screenshot can tell a running server from a
    // kernel that stopped drawing even before any client connects.
    let whole = Rect { x: 0, y: 0, w: screen.width, h: screen.height };
    compositor.composite(&mut screen, whole);
    puts("[displaysrv] the screen is mine: kernel output stopped, pixels are a process's now\n");

    let mut goodbyes = 0;
    let mut commits = 0u32;
    let mut pixels_drawn = 0usize;
    // A malformed message must not become an infinite loop: `Recv` returns at once
    // when a sender is queued, so a server that only `continue`s on rubbish spins
    // as fast as the endpoint can feed it. Bound it.
    let mut rejected = 0;
    while goodbyes < CLIENTS_EXPECTED && rejected < 8 {
        let mut msg = Message::new();
        // SAFETY: `Recv` writes one `Message` through this pointer and blocks until
        // a client sends one.
        let rc = unsafe { syscall2(SYS_RECV, EP_REQUEST, core::ptr::addr_of_mut!(msg) as u64) };
        if rc < 0 {
            puts("[displaysrv] receive failed\n");
            break;
        }
        if msg.tag == TAG_BYE {
            goodbyes += 1;
            reply(TAG_OK, 0);
            continue;
        }
        // Answered here rather than through `outcome` below because it is the one
        // request whose answer does not fit in a single word.
        if msg.tag == TAG_SCREEN {
            reply_words(
                TAG_OK,
                [
                    screen.width as u64,
                    screen.height as u64,
                    (BPP * 8) as u64,
                    FORMAT_XRGB8888,
                ],
            );
            continue;
        }

        let outcome = match msg.tag {
            TAG_CREATE => create(&mut compositor, &msg),
            TAG_COMMIT => commit(&mut compositor, &mut screen, &msg),
            TAG_RAISE => raise(&mut compositor, &mut screen, &msg),
            TAG_DESTROY => destroy(&mut compositor, &mut screen, &msg),
            _ => Err(ERR_MALFORMED),
        };
        match outcome {
            Ok(result) => {
                if msg.tag == TAG_COMMIT {
                    commits += 1;
                }
                pixels_drawn += if msg.tag == TAG_CREATE { 0 } else { result as usize };
                reply(TAG_OK, result);
            }
            Err(reason) => {
                rejected += 1;
                reply(TAG_ERROR, reason);
            }
        }
    }

    puts("[displaysrv] composited client surfaces onto a screen no client can touch\n");
    report(compositor.count, commits, pixels_drawn, rejected);
    exit();
}

/// `Create`: map the delegated buffer, check it is big enough for the geometry the
/// client claims, and put a surface at the top of the stack.
fn create(compositor: &mut Compositor, msg: &Message) -> Result<u64, u64> {
    let (width, height) = (msg.words[0] as usize, msg.words[1] as usize);
    let (x, y) = (msg.words[2] as usize, msg.words[3] as usize);
    if msg.cap == 0 || width == 0 || height == 0 {
        return Err(ERR_MALFORMED);
    }
    // The size comes from the *capability*, never from the message. A client that
    // overstates its geometry would otherwise make this process walk off the end of
    // a mapping and take the fault — the wrong process punished for a client's lie.
    // SAFETY: both syscalls only read the capability table and return a negative
    // error rather than acting on a bad handle.
    let (pages, va) = unsafe {
        (
            syscall1(SYS_SHARED_PAGES, u64::from(msg.cap)),
            syscall1(SYS_MAP_SHARED, u64::from(msg.cap)),
        )
    };
    if pages <= 0 || va < 0 {
        return Err(ERR_BUFFER);
    }
    let needed = width.checked_mul(height).and_then(|p| p.checked_mul(BPP));
    match needed {
        Some(bytes) if bytes <= pages as usize * PAGE => {}
        _ => return Err(ERR_BUFFER),
    }
    compositor
        .add(va as *const u32, width, height, x, y)
        .ok_or(ERR_FULL)
}

/// `Commit`: recomposite the rectangle the client says changed.
fn commit(compositor: &mut Compositor, screen: &mut Screen, msg: &Message) -> Result<u64, u64> {
    let index = compositor.index_of(msg.words[0]).ok_or(ERR_NO_SURFACE)?;
    let surface = compositor.surfaces[index];
    let (dx, dy) = (low(msg.words[1]), high(msg.words[1]));
    let (dw, dh) = (low(msg.words[2]), high(msg.words[2]));
    // A damage rectangle outside the surface is the client's arithmetic going
    // wrong, and the useful answer is the refusal rather than a silent clamp that
    // repaints the wrong region and looks like a rendering bug.
    if dx >= surface.width || dy >= surface.height || dw == 0 || dh == 0 {
        return Err(ERR_MALFORMED);
    }
    let rect = Rect {
        x: surface.x + dx,
        y: surface.y + dy,
        w: dw.min(surface.width - dx),
        h: dh.min(surface.height - dy),
    };
    Ok(compositor.composite(screen, rect) as u64)
}

/// `Raise`: move a surface to the top and repaint the area it covers, so the new
/// order is on the glass and not only in the list.
fn raise(compositor: &mut Compositor, screen: &mut Screen, msg: &Message) -> Result<u64, u64> {
    let index = compositor.index_of(msg.words[0]).ok_or(ERR_NO_SURFACE)?;
    compositor.raise(index);
    let surface = compositor.surfaces[compositor.count - 1];
    let rect = Rect { x: surface.x, y: surface.y, w: surface.width, h: surface.height };
    Ok(compositor.composite(screen, rect) as u64)
}

/// `Destroy`: forget the surface and repaint what it used to cover, so whatever was
/// underneath — another window, or the background — comes back.
fn destroy(compositor: &mut Compositor, screen: &mut Screen, msg: &Message) -> Result<u64, u64> {
    let index = compositor.index_of(msg.words[0]).ok_or(ERR_NO_SURFACE)?;
    let surface = compositor.surfaces[index];
    compositor.remove(index);
    let rect = Rect { x: surface.x, y: surface.y, w: surface.width, h: surface.height };
    Ok(compositor.composite(screen, rect) as u64)
}

/// The low and high halves of a packed pair. Two 32-bit values in one word because
/// a message has four, and a damage rectangle plus a surface id needs five.
fn low(word: u64) -> usize {
    (word & 0xffff_ffff) as usize
}

fn high(word: u64) -> usize {
    (word >> 32) as usize
}

/// Answer the client. A reply always goes out, refusals included: a client blocked
/// waiting for one it will never get is a hang whose cause is three messages back.
fn reply(tag: u64, result: u64) {
    reply_words(tag, [result, 0, 0, 0]);
}

/// Answer with all four words, for the requests whose answer needs them.
fn reply_words(tag: u64, words: [u64; 4]) {
    let mut msg = Message::new();
    msg.tag = tag;
    msg.words = words;
    // SAFETY: `Send` reads one `Message` through this pointer.
    unsafe {
        let _ = syscall2(SYS_SEND, EP_REPLY, core::ptr::addr_of!(msg) as u64);
    }
}

/// Print the tally, without a formatter: this program has no libc.
fn report(surfaces: usize, commits: u32, pixels: usize, rejected: u32) {
    let mut line = [0u8; 160];
    let mut n = 0;
    let mut put = |bytes: &[u8], line: &mut [u8; 160], n: &mut usize| {
        for &b in bytes {
            if *n < line.len() {
                line[*n] = b;
                *n += 1;
            }
        }
    };
    put(b"[displaysrv] ", &mut line, &mut n);
    n += number(surfaces as u64, &mut line[n..]);
    put(b" surface(s) live, ", &mut line, &mut n);
    n += number(u64::from(commits), &mut line[n..]);
    put(b" commit(s), ", &mut line, &mut n);
    n += number(pixels as u64, &mut line[n..]);
    put(b" pixel(s) composited, ", &mut line, &mut n);
    n += number(u64::from(rejected), &mut line[n..]);
    put(b" refused\n", &mut line, &mut n);
    // SAFETY: `DebugWrite` reads `n` bytes from a buffer we own.
    unsafe {
        let _ = syscall2(SYS_DEBUG_WRITE, line.as_ptr() as u64, n as u64);
    }
}

/// Write `value` in decimal at the start of `out`, returning how many bytes it
/// took. Nothing is printed if it will not fit, which is the honest failure for a
/// diagnostic: a truncated number is worse than a missing one.
fn number(value: u64, out: &mut [u8]) -> usize {
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
    if count > out.len() {
        return 0;
    }
    for i in 0..count {
        out[i] = digits[count - 1 - i];
    }
    count
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
