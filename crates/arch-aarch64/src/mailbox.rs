//! Raspberry Pi VideoCore **property mailbox** — the MMIO doorbell.
//!
//! This is the board-`unsafe` counterpart to [`staros_videocore`], which builds and
//! parses the property-tag *message*. Here we do only the mechanical part: hand the
//! GPU the physical address of a message buffer on channel
//! [`CHANNEL_PROP`](staros_videocore::CHANNEL_PROP), wait for it to answer, and make
//! the buffer coherent across that non-coherent hand-off. Everything about the
//! message layout — which is where the bugs hide — lives in the host-tested crate.
//!
//! On the QEMU `virt` machine there is no mailbox (its firmware framebuffer is
//! `ramfb` over fw_cfg instead — see [`crate::ramfb`]); [`init_framebuffer`] is the
//! path a real Pi takes, selected by the presence of a `brcm,bcm2835-mbox` node in
//! the device tree. Both produce the same [`FramebufferInfo`].
//!
//! **The pixel buffer is the GPU's, not ours.** `ALLOCATE_BUFFER` returns a base the
//! VideoCore carved from the memory split (`gpu_mem`), already reserved by firmware
//! and outside our frame pool — we only *map* it. The one page we do allocate is the
//! transient message buffer.

use core::ptr::{read_volatile, write_volatile};

use staros_mm::{PhysAddr, PAGE_SIZE};
use staros_videocore as vc;

use crate::fbinfo::FramebufferInfo;
use crate::{cache, mmu};

/// Mailbox register offsets from the `brcm,bcm2835-mbox` block base.
///
/// Read/receive uses mailbox 0 (`READ`/`STATUS0`); write/send uses mailbox 1
/// (`WRITE`). The community-standard layout polls `STATUS0` for both the full
/// (before write) and empty (before read) conditions, which is what the VideoCore
/// firmware expects.
const REG_READ: usize = 0x00;
const REG_STATUS: usize = 0x18;
const REG_WRITE: usize = 0x20;

/// `STATUS` flags.
const STATUS_FULL: u32 = 0x8000_0000;
const STATUS_EMPTY: u32 = 0x4000_0000;

/// GPU bus alias that maps RAM uncached (`0xC000_0000`). The ARM writes the *bus*
/// address of the buffer; using the uncached alias means the GPU does not read a
/// stale cache line even before our clean lands. Physical → bus for the request;
/// [`vc::bus_to_phys`] undoes it for the returned framebuffer base.
const BUS_UNCACHED: u32 = 0xC000_0000;

/// A bounded spin so a wedged mailbox fails instead of hanging early boot forever.
const SPIN_LIMIT: u32 = 1 << 24;

/// Configure a `width`×`height`, 32-bpp framebuffer via the property mailbox at
/// `mbox_phys`, allocating the small message buffer through `alloc`. Returns the
/// framebuffer geometry (the GPU may clamp the size), or `None` if the mailbox did
/// not answer, the firmware reported failure, or memory ran out.
///
/// # Safety
/// `mbox_phys` must be the MMIO base of a `brcm,bcm2835-mbox` block, covered by the
/// device linear map, and this must run single-threaded during early boot (it owns
/// the mailbox registers for the call). The returned framebuffer's frames belong to
/// firmware and must be treated as permanently reserved.
pub unsafe fn init_framebuffer(
    mbox_phys: u64,
    width: u32,
    height: u32,
    buffers: usize,
    mut alloc: impl FnMut(usize) -> Option<PhysAddr>,
) -> Option<FramebufferInfo> {
    let base = mmu::phys_to_virt(mbox_phys) as usize;

    // One page for the property message: page alignment more than satisfies the
    // 16-byte alignment the low nibble of the mailbox word steals for the channel.
    let msg = alloc(1)?;
    let msg_phys = msg.0 as u64;
    let msg_va = mmu::phys_to_virt(msg_phys) as *mut u32;

    let req = vc::FbRequest {
        width,
        height,
        depth: 32,
        pixel_order: vc::PIXEL_ORDER_RGB,
        buffers: buffers.max(1) as u32,
    };
    let (words, len_bytes) = vc::build_fb_message(&req);

    // Lay the message into the buffer.
    // SAFETY: `msg_va` is a freshly allocated, linear-mapped page we own; the
    // message is `FB_MSG_WORDS` u32s, well within a page.
    unsafe {
        for (i, w) in words.iter().enumerate() {
            write_volatile(msg_va.add(i), *w);
        }
    }
    // Publish the request to the GPU (clean), then perform the exchange.
    // SAFETY: the buffer is mapped; `len_bytes` bytes were just written.
    unsafe { cache::clean_invalidate_data(msg_va as u64, len_bytes) };

    let bus_addr = (msg_phys as u32) | BUS_UNCACHED;
    // SAFETY: `base` is the mailbox MMIO block; we own it for this call.
    if !unsafe { mailbox_exchange(base, vc::CHANNEL_PROP, bus_addr) } {
        return None;
    }

    // Observe the GPU's reply rather than a stale cached copy.
    // SAFETY: same buffer, still mapped.
    unsafe { cache::clean_invalidate_data(msg_va as u64, len_bytes) };

    // Read the (possibly rewritten) words back out and parse them.
    let mut reply = words;
    // SAFETY: reading back the same in-bounds words we wrote.
    unsafe {
        for (i, w) in reply.iter_mut().enumerate() {
            *w = read_volatile(msg_va.add(i));
        }
    }
    let fb = vc::parse_fb_response(&reply)?;

    // How many buffers we actually got, not how many were asked for. The firmware
    // clamps a virtual size it cannot back, and it does so silently: a caller that
    // trusted its own request would pan into rows outside the allocation and the
    // display would show whatever RAM is there. `size` is the allocation the GPU
    // reports, so dividing it by one screen is the firmware's own answer to the
    // question.
    let plane = fb.pitch as usize * fb.height as usize;
    let granted = (fb.size as usize)
        .checked_div(plane)
        .unwrap_or(1)
        .clamp(1, buffers.max(1));

    // SAFETY: written once during single-core early boot; this is the only writer.
    unsafe {
        MBOX = Some(Mbox {
            base,
            msg_phys,
            msg_va,
            height: fb.height,
        });
    }

    Some(FramebufferInfo {
        phys: u64::from(vc::bus_to_phys(fb.bus_base)),
        width: fb.width as usize,
        height: fb.height as usize,
        stride: fb.pitch as usize,
        buffers: granted,
    })
}

/// What [`init_framebuffer`] learned that [`set_scanout`] needs again.
struct Mbox {
    /// Linear-map address of the mailbox MMIO block.
    base: usize,
    /// The message page, kept rather than handed back: a flip is another property
    /// message, and allocating a page per frame to send it would be absurd.
    msg_phys: u64,
    msg_va: *mut u32,
    /// Physical screen height in rows — what one buffer's worth of panning is.
    height: u32,
}

/// Written once by [`init_framebuffer`] on the boot core, read by [`set_scanout`].
///
/// A plain static rather than a lock, for the reason [`crate::ramfb`] gives for
/// its own: the message page and the mailbox registers are shared, so the caller
/// serialises, and the kernel does that at the syscall.
static mut MBOX: Option<Mbox> = None;

/// Pan the display to buffer `index` — the flip, on a Pi.
///
/// Returns `false` if there is no mailbox, the GPU did not answer, or **the
/// firmware answered with a different offset than the one asked for**. That last
/// case is the one worth having a return value for: a Pi clamps a pan it cannot
/// satisfy and reports success, so a flip that quietly did nothing looks exactly
/// like a flip that worked, and the compositor goes on drawing into a buffer
/// nobody is looking at.
///
/// # Safety
/// [`init_framebuffer`] must have succeeded, and this must not run concurrently
/// with itself — it reuses one message page and the mailbox registers.
pub unsafe fn set_scanout(index: usize) -> bool {
    // SAFETY: written once during single-core boot, read-only afterwards.
    let Some(mb) = (unsafe { (&raw const MBOX).as_ref().and_then(|m| m.as_ref()) }) else {
        return false;
    };
    let want_y = mb.height * index as u32;
    let (words, len_bytes) = vc::build_offset_message(0, want_y);

    // SAFETY: `msg_va` is the linear-mapped page this module owns; the message is
    // `OFFSET_MSG_WORDS` u32s, well within a page.
    unsafe {
        for (i, w) in words.iter().enumerate() {
            write_volatile(mb.msg_va.add(i), *w);
        }
        cache::clean_invalidate_data(mb.msg_va as u64, len_bytes);
    }

    let bus_addr = (mb.msg_phys as u32) | BUS_UNCACHED;
    // SAFETY: `base` is the mailbox MMIO block; the caller serialises.
    if !unsafe { mailbox_exchange(mb.base, vc::CHANNEL_PROP, bus_addr) } {
        return false;
    }
    // SAFETY: same buffer, still mapped.
    unsafe { cache::clean_invalidate_data(mb.msg_va as u64, len_bytes) };

    let mut reply = words;
    // SAFETY: reading back the same in-bounds words we wrote.
    unsafe {
        for (i, w) in reply.iter_mut().enumerate() {
            *w = read_volatile(mb.msg_va.add(i));
        }
    }
    vc::parse_offset_response(&reply) == Some((0, want_y))
}

/// Send `value` (a 16-byte-aligned bus address) on `channel` and wait for the GPU
/// to return the same channel's reply. Returns `false` if the mailbox stays full or
/// empty past [`SPIN_LIMIT`] — a wedged GPU should not hang the kernel.
///
/// # Safety
/// `base` must be a mapped mailbox MMIO block owned by this core for the call.
unsafe fn mailbox_exchange(base: usize, channel: u32, value: u32) -> bool {
    let status = (base + REG_STATUS) as *const u32;
    let write = (base + REG_WRITE) as *mut u32;
    let read = (base + REG_READ) as *const u32;
    let msg = (value & !0xF) | (channel & 0xF);

    // Wait until the send mailbox has room, then ring the doorbell.
    // SAFETY: MMIO reads of the mailbox status register.
    if !spin_until(|| unsafe { read_volatile(status) } & STATUS_FULL == 0) {
        return false;
    }
    // SAFETY: MMIO write of the doorbell.
    unsafe { write_volatile(write, msg) };

    // Wait for a reply on our channel; other channels' replies are drained and
    // ignored (there should be none this early, but the low nibble disambiguates).
    loop {
        // SAFETY: MMIO reads of the mailbox status register.
        if !spin_until(|| unsafe { read_volatile(status) } & STATUS_EMPTY == 0) {
            return false;
        }
        // SAFETY: MMIO read of the receive mailbox.
        let resp = unsafe { read_volatile(read) };
        if resp & 0xF == channel {
            return true;
        }
    }
}

/// Spin on `ready` up to [`SPIN_LIMIT`] times; `true` if it became ready, `false` if
/// the bound was hit (a wedged mailbox must not hang the kernel forever).
fn spin_until(mut ready: impl FnMut() -> bool) -> bool {
    for _ in 0..SPIN_LIMIT {
        if ready() {
            return true;
        }
    }
    false
}

const _: () = assert!(PAGE_SIZE >= vc::FB_MSG_WORDS * 4, "message must fit one page");
