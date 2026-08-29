//! The kernel console, and the lock that stops four cores talking over it.
//!
//! The UART is one device with one data register, and until now exactly one core
//! ever wrote to it. The moment tasks ran on several cores the log started coming
//! out shredded — two messages interleaved character by character, because each
//! writer had its own `Pl011` and there was nothing to make a line atomic.
//!
//! That garbling was, briefly, the clearest evidence the cores were genuinely
//! running at the same time. It is still a bug: a kernel log you cannot read is
//! worth very little on the day you need it.
//!
//! So every kernel message goes through [`println`], which holds a lock for the
//! whole message. The lock guards *the device*, not data behind a pointer, which
//! is why it is a `SpinLock<()>` — the `Pl011` handle itself is a zero-cost
//! wrapper around a base address any core can reconstruct.
//!
//! **User space now has a line-atomic path too.** The old story here was that
//! `DebugPutc` is a one-byte syscall, so two EL0 tasks printing from two cores
//! shuffled their lines character-by-character exactly as two processes calling
//! `write(1, &c, 1)` would — and that the caller should buffer. On the UART that
//! shredding is ugly; on the *framebuffer*, where each byte also advances a shared
//! cursor, it is unreadable. So the ABI gained [`DebugWrite`](staros_abi::syscall::Syscall::DebugWrite):
//! a `(ptr, len)` syscall that emits the whole buffer through [`write_bytes`] under
//! a *single* hold of `CONSOLE`. A line printed with one `DebugWrite` cannot be
//! interrupted by another core, so the user programs build a whole line and flush
//! it in one call. `DebugPutc` stays for a lone byte; two of them from two cores
//! can still interleave, which remains the plainest evidence the cores run at once.
//!
//! The framebuffer cursor is never a *data* race regardless: [`FRAMEBUFFER`] is
//! only ever touched while `CONSOLE` is held (or during single-core install), so
//! `(col, row)` can never be corrupted by concurrent writers — the worst that a
//! bare `DebugPutc` can do is interleave whole glyphs, not tear the cursor.

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use staros_arch_aarch64::uart::Pl011;
use staros_framebuffer::Console as FbConsole;
use staros_hal::SerialConsole;

use crate::sync::SpinLock;

/// Serialises access to the UART's data register. Held for a whole message, so a
/// line from one core cannot appear inside a line from another.
static CONSOLE: SpinLock<()> = SpinLock::new(());

/// An optional graphical mirror of the console. On a machine with a framebuffer
/// (a real panel, or QEMU `ramfb`) the kernel installs one here and every message
/// is drawn to the screen as well as the UART — the only output some boards have.
/// It holds a `&'static mut` into the framebuffer, so it is only ever touched
/// while `CONSOLE` is held (or during single-core install), which is what keeps
/// the two cores from racing on the cursor.
static FRAMEBUFFER: SpinLock<Option<FbConsole<'static>>> = SpinLock::new(None);

/// Whether the kernel is still drawing to the screen.
///
/// Cleared when the screen is handed to a display server: from then on the pixels
/// belong to a process in EL0, and a kernel log line drawn over them would be
/// scribbling on someone else's window. The mirror itself is *kept*, not dropped —
/// see [`reclaim_framebuffer`], which is what a panic uses to take the screen back
/// when there is no longer anyone to be polite to.
static MIRRORING: AtomicBool = AtomicBool::new(true);

/// Install a framebuffer console as a mirror of the serial console. Called once,
/// single-core, during boot after the framebuffer is configured.
pub fn install_framebuffer(console: FbConsole<'static>) {
    *FRAMEBUFFER.lock() = Some(console);
}

/// Stop drawing kernel output to the screen: it now belongs to a display server.
///
/// The UART keeps everything. This is the whole of "the kernel gives up the
/// screen" — no unmapping, no teardown, because the mirror has to remain usable
/// for [`reclaim_framebuffer`].
pub fn stop_mirroring() {
    MIRRORING.store(false, Ordering::Release);
}

/// Take the screen back, unconditionally. For a panic: whatever a display server
/// was showing is less important than the reason the kernel is stopping, and on a
/// board whose only output is the panel, a fault report nobody can see is a fault
/// report that did not happen.
pub fn reclaim_framebuffer() {
    MIRRORING.store(true, Ordering::Release);
}

/// Whether kernel output should be drawn to the screen right now.
fn mirroring() -> bool {
    MIRRORING.load(Ordering::Acquire)
}

/// Write one raw byte to the console.
///
/// A byte, not a `char`: user space emits UTF-8 one byte at a time through
/// `DebugPutc`, and re-encoding each byte as a character would turn every
/// multi-byte sequence it prints into mojibake.
pub fn putc(byte: u8) {
    let _guard = CONSOLE.lock();
    // SAFETY: as `print`.
    let console = unsafe { Pl011::qemu_virt() };
    console.write_byte(byte);
    if mirroring() {
        if let Some(fb) = FRAMEBUFFER.lock().as_mut() {
            fb.write_byte(byte);
        }
    }
}

/// Write a whole byte buffer as one uninterruptible message.
///
/// This is what backs the `DebugWrite` syscall: the entire slice is emitted to the
/// UART and the framebuffer mirror while `CONSOLE` is held exactly once, so no
/// other core can slip a byte (or a glyph) into the middle of it. Building a line
/// in user space and handing it here in one call is how EL0 output stops being
/// shredded across cores.
pub fn write_bytes(bytes: &[u8]) {
    let _guard = CONSOLE.lock();
    // SAFETY: as `putc`.
    let console = unsafe { Pl011::qemu_virt() };
    for &byte in bytes {
        console.write_byte(byte);
    }
    if mirroring() {
        // Timed, because this is the longest uninterruptible stretch the kernel
        // has: the whole screen is copied up whenever a line reaches the bottom,
        // under this lock with interrupts masked. G3.2 measured the effect from the
        // outside — a 20 ms sleep overshooting by hundreds of milliseconds — and an
        // effect measured from the outside cannot say whether a change to the code
        // helped. This can.
        let started = staros_arch_aarch64::timer::monotonic_ns();
        if let Some(fb) = FRAMEBUFFER.lock().as_mut() {
            for &byte in bytes {
                fb.write_byte(byte);
            }
        }
        // Bytes always, nanoseconds only when there is a clock. The console runs
        // long before `init_monotonic` — the boot log's first lines are printed by
        // a kernel that cannot yet tell the time — and counting bytes only when it
        // can would report a mirror that painted nothing for the whole early boot.
        MIRROR_BYTES.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        if let (Some(a), Some(b)) = (started, staros_arch_aarch64::timer::monotonic_ns()) {
            MIRROR_NS.fetch_add(b.saturating_sub(a), Ordering::Relaxed);
            MIRROR_TIMED.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
    }
}

/// Nanoseconds spent painting the framebuffer mirror, and bytes put through it.
///
/// Only the mirror: the UART is a byte at a time into a device register and is the
/// same cost whoever owns the screen. What this measures is the part that scrolls.
static MIRROR_NS: AtomicU64 = AtomicU64::new(0);
static MIRROR_BYTES: AtomicU64 = AtomicU64::new(0);
/// Of those bytes, the ones painted while a clock existed — the only ones
/// `MIRROR_NS` describes. Reporting a rate over *all* bytes would divide a real
/// time by a count that includes the early boot it never measured.
static MIRROR_TIMED: AtomicU64 = AtomicU64::new(0);

/// `(nanoseconds, bytes painted, bytes actually timed)` for the framebuffer mirror.
#[must_use]
pub fn mirror_cost() -> (u64, u64, u64) {
    (
        MIRROR_NS.load(Ordering::Relaxed),
        MIRROR_BYTES.load(Ordering::Relaxed),
        MIRROR_TIMED.load(Ordering::Relaxed),
    )
}

/// Write `args` followed by a newline, as one uninterruptible message.
pub fn println(args: core::fmt::Arguments) {
    let _guard = CONSOLE.lock();
    // SAFETY: as `print`.
    let mut console = unsafe { Pl011::qemu_virt() };
    let _ = console.write_fmt(args);
    console.write_str("\n");
    if mirroring() {
        // Timed like `write_bytes`, because most of the boot log comes through
        // here: instrumenting only the other path measured the mirror on the lines
        // user space prints and none of the ones the kernel does, which is most of
        // the scrolling.
        let started = staros_arch_aarch64::timer::monotonic_ns();
        let mut painted = 0u64;
        if let Some(fb) = FRAMEBUFFER.lock().as_mut() {
            let before = fb.written();
            let _ = fb.write_fmt(args);
            fb.write_byte(b'\n');
            painted = fb.written() - before;
        }
        MIRROR_BYTES.fetch_add(painted, Ordering::Relaxed);
        if let (Some(a), Some(b)) = (started, staros_arch_aarch64::timer::monotonic_ns()) {
            MIRROR_NS.fetch_add(b.saturating_sub(a), Ordering::Relaxed);
            MIRROR_TIMED.fetch_add(painted, Ordering::Relaxed);
        }
    }
}

/// Single-core font self-test for the framebuffer console.
///
/// Called once during boot — after the framebuffer is installed, but before any
/// secondary core is woken or any EL0 task runs — so nothing competes for the
/// console while it prints. It sweeps the whole printable ASCII range so the 8x8
/// font can be read on a screenshot *in complete isolation* from the SMP
/// interleaving that later shares the log: if these lines render cleanly, the
/// glyph path itself is correct, and anything ragged further down is output
/// ordering between tasks, not a broken font. No-op when there is no framebuffer
/// (the sweep is a *render* test; on the UART alone it would be noise).
pub fn framebuffer_selftest() {
    if FRAMEBUFFER.lock().is_none() {
        return;
    }
    // Each line is one atomic `write_bytes`, exactly as user space now prints.
    write_bytes(b"[selftest] single-thread font render (before SMP / tasks):\n");
    write_bytes(b"  ABCDEFGHIJKLMNOPQRSTUVWXYZ\n");
    write_bytes(b"  abcdefghijklmnopqrstuvwxyz 0123456789\n");
    write_bytes(b"  !\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~\n");
    write_bytes(b"[selftest] SINGLE THREAD TEST PASSED\n");
}

/// Print a line to the kernel console, atomically against the other cores.
macro_rules! klog {
    ($($arg:tt)*) => {
        $crate::console::println(format_args!($($arg)*))
    };
}
pub(crate) use klog;
