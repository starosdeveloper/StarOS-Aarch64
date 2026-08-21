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
//!                  cap      = optional: a new buffer for this surface, swapped in
//!                             before compositing (double buffering)
//!                  -> words[0] = pixels written to the screen
//! tag = 4 Raise    words[0] = surface id              to the top of the stack
//! tag = 5 Destroy  words[0] = surface id
//! tag = 7 Watch    cap = a notification the kernel signals when this client dies
//!                  -> words[0] = 1
//! tag = 8 Focus    claim the keyboard; input events go to this client until
//!                  another claims it or this one dies
//!                  -> words[0] = 1
//! tag = 9 Bye      this client will send nothing more (its windows stay)
//! ```
//!
//! A reply is `tag = 2` with its result in `words[0]`, or `tag = 0` with a reason in
//! `words[0]` for a refusal. "Refused" and "did nothing" are different answers, and
//! a client that cannot tell them apart will one day show a blank window and call it
//! a slow frame.
//!
//! ## What happens when a client crashes
//!
//! Nothing, until it says so in advance. `Watch` hands the server a notification the
//! *kernel* signals when the client's task exits, however it exits — and the server
//! puts it in the same `WaitAny` set as the request endpoints. When it fires, that
//! client's surfaces come off the screen and the area they covered is repainted.
//!
//! The authority runs the way capabilities require: the client creates the
//! notification and delegates it. A client that never sends `Watch` is a client the
//! server cannot clean up after, and that is the honest shape — nothing here can ask
//! the kernel about a task that did not offer. A server that must not depend on
//! client goodwill has to bound what a client can hold, which is a different
//! mechanism and not this one.
//!
//! A client's windows outlive `Bye`. This server cleans up what a client *cannot*:
//! a crashed one never gets to call `Destroy`, while one that says goodbye and
//! leaves a window up has made a choice. The polite case is covered by the same
//! mechanism anyway — `NotifyOnExit` fires on an ordinary exit too.
//!
//! ## Where input goes
//!
//! `inputsrv` decodes a key and sends it here, not to an application. This is the
//! only process that knows which window is in front, so it is the only one that can
//! decide who the key belongs to; a driver that routed input would have to be told
//! about windows, and then it would be a display server with a virtqueue attached.
//!
//! Focus is **claimed**, not inferred from the stacking order. Raising a window and
//! focusing it are different acts in every real window system — a notification pops
//! to the front and must not steal the keystroke being typed — and this server has
//! no policy of its own about who deserves input: a real one takes that from the
//! shell. So the mechanism is exposed and left to be driven. The last claim wins,
//! and a client that dies loses it.
//!
//! A key arriving with nobody focused is dropped, and counted. Queueing it for the
//! next client to claim focus would deliver a keystroke to a window that was not on
//! screen when it was typed, which is worse than losing it.
//!
//! ## The pointer does not follow the focus
//!
//! A key belongs to whoever claimed the keyboard. A click belongs to whatever is
//! **under the pointer**, and those are routinely different windows — that is what
//! clicking on a window that is not focused means. So a pointer event is routed by
//! hit test: the topmost surface covering the position wins, and the position is
//! rewritten into that surface's own coordinates before it is sent, because a
//! client knows where things are inside its window and has never been told where
//! its window is.
//!
//! The position arrives as a fraction of the input device, `0..=65535` on each
//! axis, and is turned into pixels here. That split is the point: the driver is the
//! only process holding the tablet's axis range and this is the only process
//! holding the screen's size, so neither has to be told the other's number.
//!
//! Buttons are a **mask**, not events. A client that receives "button 1 went down"
//! has to remember what the others were doing to know whether this is a drag; a
//! client that receives the whole state does not, and a client that missed a
//! message recovers on the next one instead of staying wrong for ever.
//!
//! A pointer over no window at all is dropped and counted, exactly like a key with
//! nobody focused. There is no window to be wrong about.
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
//! ## Double buffering, and the vsync that is not here
//!
//! A client drawing into the buffer this server is compositing produces a torn
//! window, and no amount of care on this side fixes it: the pages are shared and
//! there is no fence. The fix belongs to the client and needs one thing from the
//! protocol — `Commit` may carry a *new* buffer, which is swapped in before the
//! rectangle is painted. So a client keeps two, draws into the one that is not on
//! screen, and commits it. That is exactly what `QBackingStore` does, and without it
//! a plugin's only options are tearing or a full-frame copy.
//!
//! A back buffer *on this side* was considered and is not here, because it buys
//! nothing: compositing computes each pixel's final colour and writes it once, so
//! there is no half-drawn pixel for a back buffer to hide, and the copy out of it
//! would tear across the rectangle exactly as the direct writes do.
//!
//! **Vsync is absent and is not simulated.** `ramfb` is one buffer with no flip and
//! no vblank — the device reads it whenever it refreshes, and there is nothing to
//! synchronise against. Anything printed here about frame timing would be a
//! measurement of QEMU's refresh loop. It waits for the board.

#![no_std]
#![no_main]

use core::arch::{asm, naked_asm};
use core::panic::PanicInfo;

use staros_virtio::{btn, AXIS_SCALE};

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
const SYS_NOTIFY_CREATE: usize = 22;
const SYS_WAIT_ANY: usize = 24;
const SYS_SHARED_PAGES: usize = 28;
const SYS_ENDPOINT_BIND: usize = 30;
const SYS_ENDPOINT_PENDING: usize = 31;
const SYS_SEND_NOWAIT: usize = 33;
const SYS_CLOCK_NOW: usize = 20;

/// How long to sleep when an event is held and there is nothing else to do.
///
/// Short enough that the second half of a click is not visibly late, long enough
/// that a stuck client does not turn this server into a spin. It is a retry
/// interval and not a timer: the ordinary case is that some other wake-up arrives
/// first and the flush happens then.
const RETRY_NS: u64 = 2_000_000;

/// A deadline `RETRY_NS` from now, or 0 for "no deadline" when there is nothing to
/// retry. Zero is what the kernel reads as "wait indefinitely".
fn retry_deadline(busy: bool) -> u64 {
    if !busy {
        return 0;
    }
    // SAFETY: `ClockNow` takes no arguments and only reads the counter.
    let now = unsafe { syscall1(SYS_CLOCK_NOW, 0) };
    if now < 0 {
        // No clock on this machine. Waiting for ever is wrong here, so ask for the
        // smallest deadline there is: it has already passed, and the wait becomes a
        // poll. That is worse than a timer and better than a lost event.
        return 1;
    }
    now as u64 + RETRY_NS
}

/// Input arrives from the driver here, and leaves for the focused client on
/// `EV_BASE + client`.
///
/// Derived from [`MAX_CLIENTS`] rather than written as 9 and 10, which is what they
/// were. The client pairs occupy handles 1 through `MAX_CLIENTS * 2`, so both of
/// these move the moment that number changes — and when they were literals, raising
/// the client ceiling silently pointed the input endpoint at a client's reply
/// channel. The symptom would have been keystrokes delivered as replies to display
/// requests, in a server that had compiled and started normally.
const EP_INPUT: u64 = (MAX_CLIENTS * 2 + 1) as u64;
const EV_BASE: u64 = EP_INPUT + 1;

/// The most client pairs this server will look for in its capability table.
///
/// The kernel grants request/reply pairs, so handle 1 is answered on handle 2 and
/// handle 3 on handle 4. A reply endpoint per client rather than one shared by all
/// of them is not bookkeeping: two clients receiving on one endpoint means either
/// may take the other's answer, and the symptom is a program acting on the reply to
/// a question it never asked — which looks like a rendering bug in whichever of
/// them noticed first.
///
/// How many clients this server *has* is not this number. It is however many of
/// those handles the kernel actually granted, which it finds out by trying to bind
/// them. A shell and an application is two clients before anything else opens a
/// window, so a server needing a rebuild to accept a third is one that will be
/// rebuilt at the worst moment.
/// Six, raised from four when the first Qt program needed a slot.
///
/// Four was already taken by this tree's own demonstrations — the framebuffer
/// client, the C program, the client that crashes on purpose, and the one that
/// receives input — so a real application had nowhere to connect. That is exactly
/// the failure the paragraph above predicted, arriving on schedule.
const MAX_CLIENTS: usize = 6;
fn request_handle(client: usize) -> u64 {
    (client * 2 + 1) as u64
}
fn reply_handle(client: usize) -> u64 {
    (client * 2 + 2) as u64
}

// Request tags.
const TAG_CREATE: u64 = 1;
const TAG_COMMIT: u64 = 3;
const TAG_RAISE: u64 = 4;
const TAG_DESTROY: u64 = 5;
const TAG_SCREEN: u64 = 6;
const TAG_WATCH: u64 = 7;
const TAG_FOCUS: u64 = 8;
const TAG_BYE: u64 = 9;

/// The only pixel format this server composites. Named in the reply so a client
/// gets told rather than assuming, and so the day a second format exists the old
/// clients are the ones that keep working.
const FORMAT_XRGB8888: u64 = 1;

// Reply tags.
const TAG_ERROR: u64 = 0;
const TAG_OK: u64 = 2;

// The tags the input driver sends here, and the two this server sends on.
/// A keyboard key: `words[0]` = code, `words[1]` = 1 down / 0 up. Forwarded to the
/// focused client unchanged — the Linux key code is what the driver decoded and
/// renumbering it here would put a translation table in every client.
const TAG_INPUT_KEY: u64 = 1;
/// From the driver: a pointer position, each axis a fraction of the device.
/// To a client: a position in *its* surface, with the whole button state.
const TAG_INPUT_POINTER: u64 = 4;
/// From the driver: one button changed. Never forwarded as itself — it changes the
/// mask, and what the client receives is a pointer message carrying that mask, so
/// that a click and the position it happened at are one message and cannot be
/// delivered out of order.
const TAG_INPUT_BUTTON: u64 = 5;

/// Which bit of the button mask each button occupies. The codes are the driver's
/// (which are Linux's); the bits are this protocol's, because a mask of raw codes
/// would be a 273-bit mask.
fn button_bit(code: u64) -> u64 {
    match code as u16 {
        btn::LEFT => 1,
        btn::RIGHT => 2,
        btn::MIDDLE => 4,
        _ => 0,
    }
}

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
///
/// Four, not "however many endpoints there are": the extra pairs exist so further
/// clients *can* connect, and waiting for goodbyes from clients that were never
/// started would be a server that never stops. The four that do say goodbye are
/// `fbclient`, `hello-c`, `qt-hello` and `shell` — the third of which only started
/// saying it the day its event loop stopped hanging, and that is what this number
/// had to be raised for once already. At two, the server counted `fbclient` and
/// `qt-hello`, printed its tally and left, and `hello-c` then found no display
/// server on a machine that had one a moment earlier. The symptom was a *C* program
/// losing its window; the cause was a *Qt* program finally finishing.
///
/// Which is the failure to expect from this number in general, and it is not
/// symmetric: too low and a client that was still drawing loses the server
/// underneath it; too high and the server waits for a goodbye that never comes,
/// holding the whole machine open until QEMU's timeout. The second is the louder of
/// the two and the first is the one that has actually happened.
const CLIENTS_EXPECTED: u32 = 4;

/// A message, laid out exactly as `staros_ipc::Message`: tag, `MESSAGE_WORDS`
/// words, then the capability handle. Four words, not six — getting that wrong
/// puts `cap` sixteen bytes past where the kernel writes it, and the symptom is a
/// server that receives real messages and calls every one of them malformed.
#[derive(Clone, Copy)]
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
    /// Which client asked for it. Kept so a dead client's windows can be taken off
    /// the screen — and so a surface id cannot be used by the client next door,
    /// which the old code allowed because ids were global and nothing checked.
    owner: usize,
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
            owner: 0,
            pixels: core::ptr::null(),
            width: 0,
            height: 0,
            x: 0,
            y: 0,
        };
        Self { surfaces: [EMPTY; MAX_SURFACES], count: 0, next_id: 1 }
    }

    /// The index of `owner`'s surface with this id, if it is live.
    ///
    /// The owner is part of the lookup and not a check bolted on afterwards. Ids
    /// are global and sequential, so a client can name its neighbour's window by
    /// adding one to its own — and until this took an owner, moving or destroying
    /// it worked.
    fn index_of(&self, owner: usize, id: u64) -> Option<usize> {
        self.surfaces[..self.count]
            .iter()
            .position(|s| s.id == id && s.owner == owner)
    }

    /// Add a surface at the top of the stack and return its id.
    fn add(
        &mut self,
        owner: usize,
        pixels: *const u32,
        width: usize,
        height: usize,
        x: usize,
        y: usize,
    ) -> Option<u64> {
        if self.count == MAX_SURFACES {
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.surfaces[self.count] = Surface { id, owner, pixels, width, height, x, y };
        self.count += 1;
        Some(id)
    }

    /// Take every surface belonging to `owner` off the stack, returning the
    /// rectangle that covers all of them — what has to be repainted, and `None` if
    /// the client had no windows.
    ///
    /// One bounding rectangle rather than one repaint per surface: two windows far
    /// apart make it wasteful and correct, and the alternative is a loop whose every
    /// step invalidates the indices of the one before it.
    fn remove_owner(&mut self, owner: usize) -> Option<Rect> {
        let mut bounds: Option<(usize, usize, usize, usize)> = None;
        let mut i = 0;
        while i < self.count {
            if self.surfaces[i].owner != owner {
                i += 1;
                continue;
            }
            let s = self.surfaces[i];
            bounds = Some(match bounds {
                None => (s.x, s.y, s.x + s.width, s.y + s.height),
                Some((x0, y0, x1, y1)) => (
                    x0.min(s.x),
                    y0.min(s.y),
                    x1.max(s.x + s.width),
                    y1.max(s.y + s.height),
                ),
            });
            self.remove(i);
        }
        bounds.map(|(x0, y0, x1, y1)| Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
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

    /// The topmost surface covering a screen position, if any.
    ///
    /// From the top down, and the first hit wins — the opposite direction to
    /// compositing, and for the same reason: the surface whose pixels the user can
    /// see at that spot is the one they meant to click. Walking bottom-up and
    /// keeping the last hit gives the same answer and costs a full scan; walking
    /// down and stopping is the answer as soon as it is known.
    ///
    /// `at()` is what decides coverage, so a click lands on a window exactly where
    /// that window is drawn. A rectangle test written separately here would be a
    /// second definition of where a window is, and the day they disagreed the
    /// symptom would be a strip along one edge that draws but cannot be clicked.
    fn topmost_at(&self, sx: usize, sy: usize) -> Option<usize> {
        (0..self.count)
            .rev()
            .find(|&i| self.surfaces[i].at(sx, sy).is_some())
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

    // One notification, both request endpoints bound to it. `Recv` blocks on one
    // endpoint, which is a server with one client; the whole reason `EndpointBind`
    // exists is that a server with two cannot be written any other way without a
    // thread per client, parked in `Recv`, forwarding messages for the sole purpose
    // of changing which primitive the wait is spelled with.
    // SAFETY: `NotifyCreate` takes no arguments and installs a capability of ours.
    let arrivals = unsafe { syscall1(SYS_NOTIFY_CREATE, 0) };
    if arrivals <= 0 {
        puts("[displaysrv] no notification to wait on\n");
        exit();
    }
    let arrivals = arrivals as u64;
    // How many clients this server can serve is a question for its capability
    // table, not for a constant. A handle the kernel never granted refuses to bind,
    // and that refusal is the answer — so the same binary serves one client on a
    // machine configured for one and four on a machine configured for four, with
    // nothing to keep in step.
    let mut clients = 0;
    for client in 0..MAX_CLIENTS {
        // SAFETY: both handles are this task's own; the kernel checks that the
        // request one carries receive rights and refuses if not.
        let rc = unsafe { syscall2(SYS_ENDPOINT_BIND, request_handle(client), arrivals) };
        if rc < 0 {
            break;
        }
        clients = client + 1;
    }
    if clients == 0 {
        puts("[displaysrv] no client endpoints; nobody can ask for a window\n");
        exit();
    }

    let mut goodbyes = 0;
    let mut commits = 0u32;
    let mut pixels_drawn = 0usize;
    // Refusals are counted for the report and **not** as a reason to stop.
    //
    // They used to bound the loop, from a version where a malformed message was
    // answered with `continue` and nothing else: `Recv` returns at once when a
    // sender is queued, so such a server spins as fast as the endpoint can feed it.
    // Every path replies now, and a client waiting for a reply is a client that
    // cannot flood — so the bound stopped protecting anything and started being a
    // hazard. A well-behaved client that makes eight mistakes is normal; a display
    // server that quits over them takes every other window down with it. This was
    // found by writing a client that exercises the refusals on purpose and getting
    // five of the eight in one run.
    let mut rejected = 0;
    // Where the next scan starts. See `next_client`.
    let mut turn = 0;
    // Each client's death notification, once it has sent `Watch`. Bound to the same
    // notification the request endpoints are, so one `WaitAny` covers messages and
    // deaths alike — a server with a second wait for deaths would be a server that
    // hears about them only when a message happens to arrive.
    let mut watches = [0u32; MAX_CLIENTS];
    let mut reaped = 0u32;
    // Who the keyboard belongs to, and how many keys have gone nowhere because
    // nobody had claimed it. Both are reported: a key that vanishes silently is the
    // hardest input bug there is to believe.
    let mut focus: Option<usize> = None;
    let mut routed = 0u32;
    let mut dropped = 0u32;
    // Where the pointer is. Not per client and not per surface: there is one on the
    // desk, and which window it is over is a question answered per event.
    let mut pointer = Pointer::new();
    // One held event per client, for the third message of a click. See `Outbox`.
    let mut outbox = Outbox::new();
    // The driver's endpoint waits in the same set as the clients'. One notification
    // for messages and deaths and input alike is the whole reason `EndpointBind`
    // exists — a second wait would mean hearing about a key only when a client
    // happened to send a request.
    // SAFETY: handle 9 is ours with receive rights; `arrivals` is our notification.
    if unsafe { syscall2(SYS_ENDPOINT_BIND, EP_INPUT, arrivals) } < 0 {
        puts("[displaysrv] no input endpoint; the keyboard will not reach a window\n");
    }
    while goodbyes < CLIENTS_EXPECTED {
        // A death is not a message, so it is checked before the queues: a client
        // that crashed after sending its last request has both waiting, and taking
        // the request first would serve a window belonging to a process that no
        // longer exists.
        while let Some(dead) = reap(&mut watches, clients) {
            if let Some(rect) = compositor.remove_owner(dead) {
                pixels_drawn += compositor.composite(&mut screen, rect);
            }
            if focus == Some(dead) {
                // The keyboard does not stay pointed at a process that is gone.
                focus = None;
            }
            reaped += 1;
            puts("[displaysrv] a client died; its windows are off the screen\n");
        }
        // Anything held from last turn goes first, so a click's release cannot
        // arrive after the press that came behind it.
        outbox.flush(clients, &mut routed);
        // Input before requests, for the same reason deaths come before both: a key
        // is a fact about the outside world, and the client it belongs to may be
        // blocked waiting for it while it sits here.
        pump_input(
            &compositor,
            &screen,
            focus,
            &mut pointer,
            &mut outbox,
            &mut routed,
            &mut dropped,
        );
        let client = match next_client(arrivals, clients, &mut turn, outbox.busy()) {
            Woken::Client(client) => client,
            // Round again: the top of this loop is where input is drained, and
            // going there is the whole point of being woken by it.
            Woken::Input => continue,
            Woken::Failed => {
                puts("[displaysrv] woken with nothing queued\n");
                break;
            }
        };
        let mut msg = Message::new();
        // SAFETY: `Recv` writes one `Message` through this pointer. A message is
        // queued at this endpoint — that is what `next_client` established — so
        // this cannot park us on an endpoint the other client is feeding.
        let rc = unsafe {
            syscall2(SYS_RECV, request_handle(client), core::ptr::addr_of_mut!(msg) as u64)
        };
        if rc < 0 {
            puts("[displaysrv] receive failed\n");
            break;
        }
        if msg.tag == TAG_BYE {
            goodbyes += 1;
            // `Bye` does **not** take the client's windows down, and that is a
            // decision rather than an omission.
            //
            // The rule this server follows is that it cleans up what a client
            // *cannot*. A crashed client never gets to call `Destroy`; a client that
            // says goodbye and leaves its windows up made a choice, and taking them
            // away would make `Destroy` unreachable in practice. The cleanup that
            // covers the polite case is the same one that covers the crash —
            // `NotifyOnExit` fires on an ordinary `Exit` too, so a client that sent
            // `Watch` is tidied up either way, and one that did not is a client
            // that asked for its windows to outlive it.
            reply(client, TAG_OK, 0);
            continue;
        }
        // Answered here rather than through `outcome` below because it is the one
        // request whose answer does not fit in a single word.
        if msg.tag == TAG_SCREEN {
            reply_words(
                client,
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
            TAG_CREATE => create(&mut compositor, client, &msg),
            TAG_COMMIT => commit(&mut compositor, &mut screen, client, &msg),
            TAG_RAISE => raise(&mut compositor, &mut screen, client, &msg),
            TAG_DESTROY => destroy(&mut compositor, &mut screen, client, &msg),
            TAG_WATCH => watch(&mut watches, client, arrivals, &msg),
            TAG_FOCUS => {
                focus = Some(client);
                Ok(1)
            }
            _ => Err(ERR_MALFORMED),
        };
        match outcome {
            Ok(result) => {
                if msg.tag == TAG_COMMIT {
                    commits += 1;
                }
                pixels_drawn += match msg.tag {
                    TAG_CREATE | TAG_WATCH | TAG_FOCUS => 0,
                    _ => result as usize,
                };
                reply(client, TAG_OK, result);
            }
            Err(reason) => {
                rejected += 1;
                reply(client, TAG_ERROR, reason);
            }
        }
    }

    puts("[displaysrv] composited client surfaces onto a screen no client can touch\n");
    report(compositor.count, commits, pixels_drawn, rejected, reaped, routed, dropped);

    // The tally is printed and this server does **not** exit.
    //
    // The goodbyes say the drawing clients are finished; they say nothing about the
    // keyboard, and a key can be pressed at any moment for as long as the machine
    // runs. A server that stopped here was one that had already gone by the time
    // anyone pressed anything — which is what happened, and the symptom was a
    // decoded key press with nowhere to go.
    //
    // Living forever is what the UART driver already does, and the shutdown path
    // accounts for it: the kernel reports how many tasks are still alive and holding
    // an address space. One more of those is the price of a display server that is
    // there when the user is.
    loop {
        while let Some(dead) = reap(&mut watches, clients) {
            if let Some(rect) = compositor.remove_owner(dead) {
                compositor.composite(&mut screen, rect);
            }
            if focus == Some(dead) {
                focus = None;
            }
        }
        outbox.flush(clients, &mut routed);
        pump_input(
            &compositor,
            &screen,
            focus,
            &mut pointer,
            &mut outbox,
            &mut routed,
            &mut dropped,
        );
        let handles = [arrivals as u32];
        let deadline = retry_deadline(outbox.busy());
        // SAFETY: `WaitAny` reads one handle from this array and parks; the array
        // outlives the call.
        let rc = unsafe {
            syscall3(SYS_WAIT_ANY, handles.as_ptr() as u64, handles.len() as u64, deadline)
        };
        // An expired deadline is the retry, not a failure; only an indefinite wait
        // returning an error means there is nothing left to wait on.
        if rc < 0 && deadline == 0 {
            exit();
        }
    }
}

/// `Create`: map the delegated buffer, check it is big enough for the geometry the
/// client claims, and put a surface at the top of the stack.
fn create(compositor: &mut Compositor, owner: usize, msg: &Message) -> Result<u64, u64> {
    let (width, height) = (msg.words[0] as usize, msg.words[1] as usize);
    let (x, y) = (msg.words[2] as usize, msg.words[3] as usize);
    if width == 0 || height == 0 {
        return Err(ERR_MALFORMED);
    }
    let pixels = accept_buffer(msg.cap, width, height)?;
    compositor.add(owner, pixels, width, height, x, y).ok_or(ERR_FULL)
}

/// Map a delegated buffer and check it really holds `width * height` pixels.
///
/// The size comes from the *capability*, never from the message. A client that
/// overstates its geometry would otherwise make this process walk off the end of a
/// mapping and take the fault — the wrong process punished for a client's lie, and
/// a display server that dies takes every window with it.
///
/// Shared by `Create` and `Commit`, which is the point: the second buffer of a
/// double-buffered surface arrives through a different request and must be checked
/// exactly as hard as the first. A check written once cannot be forgotten in one of
/// two places.
fn accept_buffer(cap: u32, width: usize, height: usize) -> Result<*const u32, u64> {
    if cap == 0 {
        return Err(ERR_MALFORMED);
    }
    // SAFETY: both syscalls only read the capability table and return a negative
    // error rather than acting on a bad handle.
    let (pages, va) = unsafe {
        (
            syscall1(SYS_SHARED_PAGES, u64::from(cap)),
            syscall1(SYS_MAP_SHARED, u64::from(cap)),
        )
    };
    if pages <= 0 || va < 0 {
        return Err(ERR_BUFFER);
    }
    let needed = width.checked_mul(height).and_then(|p| p.checked_mul(BPP));
    match needed {
        Some(bytes) if bytes <= pages as usize * PAGE => Ok(va as *const u32),
        _ => Err(ERR_BUFFER),
    }
}

/// `Commit`: recomposite the rectangle the client says changed.
fn commit(
    compositor: &mut Compositor,
    screen: &mut Screen,
    owner: usize,
    msg: &Message,
) -> Result<u64, u64> {
    let index = compositor.index_of(owner, msg.words[0]).ok_or(ERR_NO_SURFACE)?;
    // A buffer with the commit means the client has been drawing somewhere else and
    // wants that shown instead. Swapped in *before* the rectangle is painted, and
    // checked against this surface's geometry first: a second buffer is as good a
    // place to lie about a size as the first.
    if msg.cap != 0 {
        let s = compositor.surfaces[index];
        compositor.surfaces[index].pixels = accept_buffer(msg.cap, s.width, s.height)?;
    }
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
fn raise(
    compositor: &mut Compositor,
    screen: &mut Screen,
    owner: usize,
    msg: &Message,
) -> Result<u64, u64> {
    let index = compositor.index_of(owner, msg.words[0]).ok_or(ERR_NO_SURFACE)?;
    compositor.raise(index);
    let surface = compositor.surfaces[compositor.count - 1];
    let rect = Rect { x: surface.x, y: surface.y, w: surface.width, h: surface.height };
    Ok(compositor.composite(screen, rect) as u64)
}

/// `Destroy`: forget the surface and repaint what it used to cover, so whatever was
/// underneath — another window, or the background — comes back.
fn destroy(
    compositor: &mut Compositor,
    screen: &mut Screen,
    owner: usize,
    msg: &Message,
) -> Result<u64, u64> {
    let index = compositor.index_of(owner, msg.words[0]).ok_or(ERR_NO_SURFACE)?;
    let surface = compositor.surfaces[index];
    compositor.remove(index);
    let rect = Rect { x: surface.x, y: surface.y, w: surface.width, h: surface.height };
    Ok(compositor.composite(screen, rect) as u64)
}

/// `Watch`: take the notification a client delegated and bind it into the same
/// wait set the request endpoints use, so a death and a message wake this server
/// the same way.
///
/// The handle arrives *because the client sent it* — the kernel installed it here
/// when the message was delivered. Nothing in this server can ask to watch a task
/// that did not offer, which is the property that makes this safe to accept from
/// anyone: the worst a client can do is ask to be watched.
fn watch(watches: &mut [u32; MAX_CLIENTS], client: usize, _arrivals: u64, msg: &Message)
    -> Result<u64, u64>
{
    if msg.cap == 0 {
        return Err(ERR_MALFORMED);
    }
    watches[client] = msg.cap;
    Ok(1)
}

/// Where the pointer is on the screen and which of its buttons are down.
///
/// One pointer for the machine, not one per client: there is one on the desk. It
/// lives here rather than in the driver because a position in pixels needs the
/// screen, and the driver has never been told how big it is.
struct Pointer {
    x: usize,
    y: usize,
    buttons: u64,
    /// Whether a position has ever arrived. A button that clicks before anything
    /// has moved has no position to happen at, and (0, 0) is a lie that lands on
    /// whatever window is in the corner.
    seen: bool,
}

impl Pointer {
    const fn new() -> Self {
        Self { x: 0, y: 0, buttons: 0, seen: false }
    }
}

/// Turn one axis of a device-relative position into a pixel on this screen.
///
/// `extent - 1` and not `extent`: the far edge of the device is the *last* pixel,
/// not one past it. Scaling by the width would put the rightmost position outside
/// the screen, where the hit test finds nothing and a click at the edge of a
/// maximised window does nothing at all.
fn to_pixels(fraction: u64, extent: usize) -> usize {
    if extent == 0 {
        return 0;
    }
    let f = fraction.min(u64::from(AXIS_SCALE));
    (f * (extent as u64 - 1) / u64::from(AXIS_SCALE)) as usize
}

/// Read everything the driver has sent and route it: keys by focus, the pointer by
/// what is under it.
fn pump_input(
    compositor: &Compositor,
    screen: &Screen,
    focus: Option<usize>,
    pointer: &mut Pointer,
    outbox: &mut Outbox,
    routed: &mut u32,
    dropped: &mut u32,
) {
    while let Some(event) = take_input() {
        let delivered = match event.tag {
            TAG_INPUT_KEY => match focus {
                Some(client) => outbox.send(client, &event),
                None => Delivery::Dropped,
            },
            TAG_INPUT_POINTER => {
                pointer.x = to_pixels(event.words[0], screen.width);
                pointer.y = to_pixels(event.words[1], screen.height);
                pointer.seen = true;
                deliver_pointer(compositor, pointer, outbox)
            }
            TAG_INPUT_BUTTON => {
                let bit = button_bit(event.words[0]);
                if event.words[1] != 0 {
                    pointer.buttons |= bit;
                } else {
                    pointer.buttons &= !bit;
                }
                // The button is delivered as a pointer message at the position it
                // happened at, so a client cannot receive a click before the move
                // that put the pointer where it was clicked.
                deliver_pointer(compositor, pointer, outbox)
            }
            // An event kind this server does not route. Counted, not ignored: a
            // driver sending something nobody handles is a fact worth one number.
            _ => Delivery::Dropped,
        };
        match delivered {
            Delivery::Sent => *routed += 1,
            Delivery::Held => {}
            Delivery::Dropped => *dropped += 1,
        }
    }
}

/// Whether the first pointer event has been announced. The line is printed once,
/// because a pointer produces one of these per movement and a log that carried them
/// all would be a log about the mouse.
static mut POINTER_ANNOUNCED: bool = false;

/// Send the pointer's state to whoever owns the surface under it, in that
/// surface's coordinates. `false` if there is no window there.
fn deliver_pointer(compositor: &Compositor, pointer: &Pointer, outbox: &mut Outbox) -> Delivery {
    if !pointer.seen {
        return Delivery::Dropped;
    }
    let Some(index) = compositor.topmost_at(pointer.x, pointer.y) else {
        return Delivery::Dropped;
    };
    let surface = compositor.surfaces[index];
    // SAFETY: this server is one EL0 task, so its code runs on one core at a time
    // and nothing races this flag.
    if !unsafe { POINTER_ANNOUNCED } {
        // SAFETY: as above.
        unsafe { POINTER_ANNOUNCED = true };
        announce_pointer(pointer.x, pointer.y, surface.id, surface.owner);
    }
    let mut msg = Message::new();
    msg.tag = TAG_INPUT_POINTER;
    // Surface-local first, because that is the one a client acts on. Screen
    // coordinates come too: a client dragging a window needs to know where the
    // pointer is in the space the window moves through, and computing it from the
    // local position means knowing where its own window is, which is the one thing
    // this protocol never tells it.
    msg.words[0] = pack(pointer.x - surface.x, pointer.y - surface.y);
    msg.words[1] = pack(pointer.x, pointer.y);
    msg.words[2] = pointer.buttons;
    msg.words[3] = surface.id;
    outbox.send(surface.owner, &msg)
}

/// Say, once, where the pointer landed and what it landed on.
///
/// One line built in one buffer, for the reason `inputsrv` learned twice: three
/// `DebugWrite` calls are three chances for another task to write between them, and
/// a line that is *usually* whole is the worst kind of evidence.
fn announce_pointer(x: usize, y: usize, surface: u64, owner: usize) {
    let mut line = [0u8; 160];
    let mut n = 0;
    let put = |bytes: &[u8], line: &mut [u8; 160], n: &mut usize| {
        for &b in bytes {
            if *n < line.len() {
                line[*n] = b;
                *n += 1;
            }
        }
    };
    put(b"[displaysrv] pointer at (", &mut line, &mut n);
    n += number(x as u64, &mut line[n..]);
    put(b", ", &mut line, &mut n);
    n += number(y as u64, &mut line[n..]);
    put(b") landed on surface ", &mut line, &mut n);
    n += number(surface, &mut line[n..]);
    put(b" of client ", &mut line, &mut n);
    n += number(owner as u64, &mut line[n..]);
    put(b" - routed by what is under it, not by who has the keyboard\n", &mut line, &mut n);
    // SAFETY: `DebugWrite` reads `n` bytes from a buffer we own.
    unsafe {
        let _ = syscall2(SYS_DEBUG_WRITE, line.as_ptr() as u64, n as u64);
    }
}

/// Two 32-bit values in one word, the same packing the damage rectangle uses.
fn pack(low: usize, high: usize) -> u64 {
    (low as u64 & 0xffff_ffff) | ((high as u64) << 32)
}

/// Take one input event from the driver, if one is waiting — non-blocking.
///
/// `EndpointPending` first and `Recv` only when it says there is something: `Recv`
/// blocks, and a server that blocked here would stop serving windows the moment the
/// keyboard went quiet, which is most of the time.
fn take_input() -> Option<Message> {
    // SAFETY: handle 9 is ours with receive rights; this only reads a queue length.
    if unsafe { syscall1(SYS_ENDPOINT_PENDING, EP_INPUT) } <= 0 {
        return None;
    }
    let mut msg = Message::new();
    // SAFETY: a message is queued, so this cannot park us; `Recv` writes one
    // `Message` through the pointer.
    let rc = unsafe { syscall2(SYS_RECV, EP_INPUT, core::ptr::addr_of_mut!(msg) as u64) };
    (rc >= 0).then_some(msg)
}

/// How many events this server will hold for one client that is not keeping up.
///
/// Eight, and the number comes from a burst rather than from taste. Pressing a key
/// and clicking a button at nearly the same moment is five messages — key down, key
/// up, the pointer's move, its press and its release — and they arrive at this
/// server in one go, because the driver hands over everything its device produced
/// before going back to sleep. A client's endpoint ring holds two. So without room
/// for the rest, three of the five are lost, and the ones lost are whichever came
/// last: the release, which is the half of a click that makes it a click.
///
/// It is a bound and not a buffer. A client that cannot take eight events in a turn
/// of this loop is a client whose input is already stale, and the ninth is dropped
/// and counted rather than waited for — because waiting is what stops the screen.
const OUTBOX_DEPTH: usize = 8;

/// Events per client that would not fit when they were produced.
///
/// An endpoint's ring holds two messages, deliberately — it is tiny so that the
/// blocking-send path gets exercised rather than hidden. A single click is *three*
/// messages: the move that put the pointer there, the press, and the release. So
/// the third one of an ordinary click meets a full ring, and with `SendNoWait` it
/// would simply be lost.
///
/// Losing which one matters. A dropped position corrects itself on the next
/// movement — the message carries the whole state, which is why it does. A dropped
/// **release** does not: the client is left holding a button nobody let go of, and
/// the click it was half-way through never completes. That is what happened, and
/// what it looked like was a button that highlighted and never fired.
///
/// So a short queue per client, flushed at the top of every turn of the loop — by
/// which time the client has usually drained a message and there is room. When even
/// that is full the *newest* event is dropped rather than the oldest, because order
/// is the one thing a stream of button transitions cannot survive losing: a release
/// delivered before its press leaves a client holding a button for ever.
struct Outbox {
    queues: [[Message; OUTBOX_DEPTH]; MAX_CLIENTS],
    /// Where each client's queue starts and how much of it is in use.
    head: [usize; MAX_CLIENTS],
    len: [usize; MAX_CLIENTS],
}

impl Outbox {
    const fn new() -> Self {
        Self {
            queues: [[Message::new(); OUTBOX_DEPTH]; MAX_CLIENTS],
            head: [0; MAX_CLIENTS],
            len: [0; MAX_CLIENTS],
        }
    }

    /// Try to deliver everything held for each client, oldest first, stopping at the
    /// first one that will not fit. Called before anything else each turn, so held
    /// events go out ahead of the ones behind them.
    fn flush(&mut self, clients: usize, routed: &mut u32) {
        for client in 0..clients {
            while self.len[client] > 0 {
                let event = self.queues[client][self.head[client]];
                if !push(client, &event) {
                    break;
                }
                self.head[client] = (self.head[client] + 1) % OUTBOX_DEPTH;
                self.len[client] -= 1;
                *routed += 1;
            }
        }
    }

    /// Whether anything is waiting to go out.
    ///
    /// The waits below consult this. A held event is not woken by anything — the
    /// client draining its endpoint signals nobody — so a server that parked with no
    /// deadline would sit on the last event of a burst until the next unrelated
    /// thing happened. In a click that is the release, and it would arrive whenever
    /// the pointer next moved, which may be never.
    fn busy(&self) -> bool {
        self.len.iter().any(|&n| n > 0)
    }

    /// Send now, queue it for the next turn, or drop it.
    ///
    /// Anything already queued goes first even if the ring has room, because sending
    /// this one past it would reorder them.
    fn send(&mut self, client: usize, event: &Message) -> Delivery {
        if self.len[client] == 0 && push(client, event) {
            return Delivery::Sent;
        }
        if self.len[client] == OUTBOX_DEPTH {
            return Delivery::Dropped;
        }
        let tail = (self.head[client] + self.len[client]) % OUTBOX_DEPTH;
        self.queues[client][tail] = *event;
        self.len[client] += 1;
        Delivery::Held
    }
}

/// What became of one event. Three outcomes and not two, because "held" is neither
/// of the others: counting it as routed would count it twice when it goes out, and
/// counting it as dropped would report a loss that did not happen.
enum Delivery {
    Sent,
    Held,
    Dropped,
}

/// Hand an event to a client, unchanged, and say whether it fitted.
///
/// Unchanged is the decision: the driver's tag is the Linux event kind, and a
/// server that renumbered them would make every client carry a translation table
/// for a mapping that already existed. The event is not tagged with a window
/// either — the client has the focus or it does not, and a window id here would be
/// a second answer to a question the focus already settled.
///
/// **`SendNoWait`, never `Send`.** This is the only place in this server that
/// pushes to a program which asked for nothing, and several clients in this tree
/// never read their event endpoint at all. A blocking send to one of them parks the
/// compositor for ever — the screen stops, every other window stops with it, and the
/// input driver blocks behind it on the next event. That is not a hypothetical: it
/// is what a pointer moving over `fbclient` did, and the symptom was a keyboard that
/// worked once and then went quiet, three processes away from the cause.
fn push(client: usize, event: &Message) -> bool {
    // SAFETY: `SendNoWait` reads one `Message` through this pointer, to a send-only
    // endpoint this task holds, and never parks us.
    let rc = unsafe {
        syscall2(
            SYS_SEND_NOWAIT,
            EV_BASE + client as u64,
            core::ptr::from_ref(event) as u64,
        )
    };
    rc >= 0
}

/// Which client has died since this was last asked, if any — non-blocking.
///
/// `WaitAny` with a deadline already in the past is the non-blocking form: it
/// consumes a pending signal and returns its index, or reports that it would have
/// blocked. A deadline of zero means "no deadline" to the kernel, so the value here
/// is 1 nanosecond, which is in the past on every machine that has finished booting.
fn reap(watches: &mut [u32; MAX_CLIENTS], clients: usize) -> Option<usize> {
    for client in 0..clients {
        let handle = watches[client];
        if handle == 0 {
            continue;
        }
        let handles = [handle];
        // SAFETY: `WaitAny` reads one handle from this array and returns at once
        // because the deadline has passed; the array outlives the call.
        let rc = unsafe {
            syscall3(SYS_WAIT_ANY, handles.as_ptr() as u64, handles.len() as u64, 1)
        };
        if rc >= 0 {
            // Once. A dead client cannot die again, and leaving the registration in
            // place would have the next scan report the same death for ever if the
            // kernel ever counted two signals.
            watches[client] = 0;
            return Some(client);
        }
    }
    None
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
fn reply(client: usize, tag: u64, result: u64) {
    reply_words(client, tag, [result, 0, 0, 0]);
}

/// Answer with all four words, for the requests whose answer needs them.
fn reply_words(client: usize, tag: u64, words: [u64; 4]) {
    let mut msg = Message::new();
    msg.tag = tag;
    msg.words = words;
    // SAFETY: `Send` reads one `Message` through this pointer, to the endpoint
    // paired with the one the request came from.
    unsafe {
        let _ = syscall2(SYS_SEND, reply_handle(client), core::ptr::addr_of!(msg) as u64);
    }
}

/// Why this server woke up.
///
/// Three answers and not two, and the third is the one that was missing. `Recv` on a
/// client endpoint is the only *blocking* thing this loop does, so anything that has
/// to be noticed has to be a reason to stop waiting — and input was not one. The
/// server slept until a client sent a request, which is fine while clients are busy
/// and is a keyboard that stops working the moment they go quiet: the driver blocks
/// in `Send` on a full endpoint, its interrupt goes unacknowledged, and the device
/// falls silent. What that looks like from the outside is a test that passes when the
/// key is pressed early in the boot and fails when it is pressed late.
enum Woken {
    /// This client has a request queued.
    Client(usize),
    /// The driver sent something; go round and drain it.
    Input,
    /// The wait itself failed; there is nothing left to serve.
    Failed,
}

/// Wait until some client has a message queued, and return which.
///
/// The notification says *something* arrived, never what: `WaitAny` consumes the
/// signal it reports, so treating it as a per-endpoint readiness would mean
/// remembering a bit — and a remembered readiness is how a server comes to pick an
/// endpoint that has nothing and block there while the other client waits for an
/// answer. So the notification only ends the sleep, and the queues are asked
/// afresh, in order, every time round.
///
/// Round robin, not "scan from zero". `turn` is where the scan starts and it moves
/// past whoever was just served, so a client that always has a request queued
/// cannot hold the server while another waits.
///
/// This is not a refinement of a working policy — starting from zero every time is
/// starvation with two clients and a busy first one, and the shape it takes is a
/// window that never repaints while another animates smoothly. Cheap enough that
/// there is no reason to leave the unfair version in and find out later which of
/// the two windows was the shell.
fn next_client(arrivals: u64, clients: usize, turn: &mut usize, busy: bool) -> Woken {
    loop {
        // Input first, and before the clients rather than after them. A key is a
        // fact about the outside world with a person behind it; a request is a
        // program that will wait. And the driver is *blocked* while its message
        // sits here — an endpoint's ring is small — so a server that served every
        // queued request before looking is a server that stops the keyboard for the
        // length of a busy frame.
        // SAFETY: `EndpointPending` only reads the queue length of a handle we hold
        // with receive rights.
        if unsafe { syscall1(SYS_ENDPOINT_PENDING, EP_INPUT) } > 0 {
            return Woken::Input;
        }
        for step in 0..clients {
            let client = (*turn + step) % clients;
            // SAFETY: as above.
            let pending = unsafe { syscall1(SYS_ENDPOINT_PENDING, request_handle(client)) };
            if pending > 0 {
                *turn = (client + 1) % clients;
                return Woken::Client(client);
            }
        }
        let handles = [arrivals as u32];
        let deadline = retry_deadline(busy);
        // SAFETY: `WaitAny` reads one handle from this array; the array outlives the
        // call.
        let rc = unsafe {
            syscall3(SYS_WAIT_ANY, handles.as_ptr() as u64, handles.len() as u64, deadline)
        };
        // A deadline that expired is not a failure — it is the retry this server
        // asked for. Only a wait with no deadline can fail its way out of the loop.
        if rc < 0 {
            if deadline != 0 {
                return Woken::Input;
            }
            return Woken::Failed;
        }
    }
}

/// Print the tally, without a formatter: this program has no libc.
fn report(
    surfaces: usize,
    commits: u32,
    pixels: usize,
    rejected: u32,
    reaped: u32,
    routed: u32,
    dropped: u32,
) {
    // 256 and not 160. The old buffer fitted the old wording exactly, and naming
    // the counters properly — "input event(s)" rather than "key(s)", "for want of a
    // window" rather than "of focus" — pushed the line past the end. Nothing said
    // so: `put` stops at the end of the buffer and `number` writes nothing when it
    // will not fit, so the report simply ended mid-word. The smoke matrix caught it
    // because it asserts the whole line; a person reading the console would have
    // seen a tally that looked complete.
    let mut line = [0u8; 256];
    let mut n = 0;
    let put = |bytes: &[u8], line: &mut [u8; 256], n: &mut usize| {
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
    put(b" refused, ", &mut line, &mut n);
    n += number(u64::from(reaped), &mut line[n..]);
    put(b" client(s) reaped, ", &mut line, &mut n);
    n += number(u64::from(routed), &mut line[n..]);
    put(b" input event(s) routed, ", &mut line, &mut n);
    n += number(u64::from(dropped), &mut line[n..]);
    put(b" dropped for want of a window\n", &mut line, &mut n);
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

/// A three-argument syscall. Only `WaitAny` needs the third register.
///
/// # Safety
/// As [`syscall1`].
unsafe fn syscall3(number: usize, a0: u64, a1: u64, a2: u64) -> isize {
    let ret;
    // SAFETY: as `syscall2`, with a third argument in x2.
    unsafe {
        asm!(
            "svc #0",
            in("x8") number,
            inout("x0") a0 => ret,
            in("x1") a1,
            in("x2") a2,
            options(nostack),
        );
    }
    ret
}

/// Nothing here panics deliberately; the lang item has to exist regardless.
#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    exit()
}
