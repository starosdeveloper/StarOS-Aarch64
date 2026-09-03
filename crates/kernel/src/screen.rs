//! Which buffer the display controller is scanning out, and the one operation
//! that changes it.
//!
//! The framebuffer itself belongs to user space — the kernel maps the pixels into
//! the display server and stops drawing (see `console::install_framebuffer` and the
//! hand-off in `main`). What cannot be handed over is the *scanout base*: it is a
//! register on a device, reached through fw_cfg on QEMU and a GPU mailbox on a Pi,
//! and neither is something a process can be given a page of. So the pixels are
//! user space's and the flip is a syscall, and this module is the whole of the
//! kernel's side of it.
//!
//! Three things live here that do not belong in either arch driver:
//!
//! * **Which source is live.** `ramfb` and the VideoCore mailbox are chosen at boot
//!   by what the device tree says; a flip has to reach the one that answered.
//! * **Serialisation.** Both drivers reuse a single scratch page and a fixed pair of
//!   MMIO registers, and both say so in their safety notes. Two cores flipping at
//!   once would interleave one descriptor into another. The lock is here because
//!   this is where the two drivers meet.
//! * **The count.** How many flips actually happened is the only number that
//!   distinguishes double buffering from a second buffer nobody ever shows.

use core::sync::atomic::{AtomicU64, Ordering};

use staros_arch_aarch64::fbinfo::FramebufferInfo;

use crate::sync::SpinLock;

/// The screen this kernel brought up, or `None` on a machine with no framebuffer.
///
/// Behind the same lock that serialises the flip, so that a reader cannot see the
/// geometry change under a transfer. It never actually changes after boot; the lock
/// is for the flip, and putting the info under it costs nothing and removes a
/// second thing to reason about.
static SCREEN: SpinLock<Option<Screen>> = SpinLock::new(None);

/// How many times the scanout base has actually moved.
///
/// The claim, and the one number that fails if the flip is a no-op: a compositor
/// can draw into a back buffer, report a frame and look entirely correct while the
/// display shows the other buffer for ever. Counted here, at the syscall, rather
/// than in the server, because a server counting its own requests would count the
/// ones the kernel refused.
static FLIPS: AtomicU64 = AtomicU64::new(0);
/// Flip requests refused: no screen, no such buffer, or the device said no.
static REFUSED: AtomicU64 = AtomicU64::new(0);

/// Which driver answered at boot.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// QEMU `ramfb`, reconfigured over fw_cfg.
    Ramfb,
    /// A Raspberry Pi's VideoCore, panned over the property mailbox.
    Mailbox,
}

/// The live screen.
struct Screen {
    info: FramebufferInfo,
    source: Source,
    /// The buffer currently being scanned out.
    shown: usize,
}

/// Record the framebuffer the kernel brought up and the source that produced it.
///
/// `source` is the same string `main` prints, matched rather than passed as an enum
/// because that string is already the single place the choice is made and a second
/// encoding of it would be a second thing to keep in step. An unrecognised source
/// leaves the screen unregistered: flips are then refused, which is the honest
/// answer for a framebuffer this module does not know how to re-point.
pub fn adopt(info: FramebufferInfo, source: &str) {
    let source = match source {
        "ramfb" => Source::Ramfb,
        "VideoCore mailbox" => Source::Mailbox,
        _ => return,
    };
    *SCREEN.lock() = Some(Screen { info, source, shown: 0 });
}

/// Scan out buffer `index`. Returns the index now on screen, or [`None`] if the
/// request was refused.
///
/// Refused rather than clamped for an out-of-range index. A compositor that asked
/// for buffer 2 on a two-buffer screen has a bug, and silently showing it buffer 1
/// would leave that bug drawing into memory nobody displays with nothing anywhere
/// saying so.
///
/// A flip to the buffer already shown is *not* a no-op that reports success
/// cheaply: it goes to the device like any other. The device is the only thing that
/// knows what it is scanning out, and a kernel that answered from its own cached
/// `shown` would keep answering correctly after the two had drifted apart.
pub fn flip(index: usize) -> Option<usize> {
    let mut guard = SCREEN.lock();
    let Some(screen) = guard.as_mut() else {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    if index >= screen.info.buffers {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    // SAFETY: the matching driver initialised successfully — that is what `adopt`
    // records — and the scheduler lock we hold is the serialisation both drivers
    // require. Neither touches memory belonging to a task.
    let ok = unsafe {
        match screen.source {
            Source::Ramfb => staros_arch_aarch64::ramfb::set_scanout(index),
            Source::Mailbox => staros_arch_aarch64::mailbox::set_scanout(index),
        }
    };
    if !ok {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    screen.shown = index;
    FLIPS.fetch_add(1, Ordering::Relaxed);
    Some(index)
}

/// Flips performed, flips refused, how many buffers the screen has, and which one
/// is on it.
///
/// The buffer count is the context and the flip count is the claim. Two buffers
/// with zero flips is a kernel that reserved twice the memory and displayed one
/// half of it for the whole boot — which looks identical, in every log line and
/// every screenshot, to double buffering that works.
#[must_use]
pub fn stats() -> (u64, u64, usize, usize) {
    let guard = SCREEN.lock();
    let (buffers, shown) = guard.as_ref().map_or((0, 0), |s| (s.info.buffers, s.shown));
    (
        FLIPS.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
        buffers,
        shown,
    )
}
