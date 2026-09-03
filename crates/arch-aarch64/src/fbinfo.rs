//! The kernel's view of a configured framebuffer.
//!
//! Whatever lit the screen — QEMU `ramfb` over fw_cfg, a Raspberry Pi's VideoCore
//! mailbox, a laptop's UEFI GOP — the rest of the kernel only ever sees these four
//! numbers and builds the portable console (`staros_framebuffer`) from them. Each
//! source is a different arch path producing the *same* [`FramebufferInfo`].

/// A linear (packed-pixel) framebuffer the firmware or a scanout engine handed us.
#[derive(Clone, Copy, Debug)]
pub struct FramebufferInfo {
    /// Physical base of the pixel buffer (reachable through the linear map).
    pub phys: u64,
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// Bytes per row (may exceed `width * bytes_per_pixel`).
    pub stride: usize,
    /// How many full-screen buffers the allocation holds, stacked one after
    /// another: buffer `i` starts at `phys + i * height * stride`.
    ///
    /// One means there is nowhere to draw except the pixels being scanned out, and
    /// a compositor writing a frame is racing the display controller reading it.
    /// Two means it can draw the next frame somewhere invisible and then ask for
    /// the scanout to move — which is the only thing "double buffered" means at
    /// this layer.
    ///
    /// Stacked in *one* allocation rather than kept as two independent buffers,
    /// and that is not an implementation detail: it is the shape both hardware
    /// paths want. A Pi pans within one taller virtual framebuffer
    /// (`SET_VIRTUAL_OFFSET` moves a y origin, it does not take an address), and
    /// `ramfb` takes a base address that this makes trivial arithmetic. A pair of
    /// separate runs would serve `ramfb` and would have to be undone for the board.
    pub buffers: usize,
}

impl FramebufferInfo {
    /// Byte offset of buffer `index` from [`phys`](FramebufferInfo::phys).
    ///
    /// Not bounds-checked against [`buffers`](FramebufferInfo::buffers): callers
    /// that take the index from user space must check it first, and the one that
    /// does is the flip syscall.
    #[must_use]
    pub const fn plane_offset(&self, index: usize) -> usize {
        index * self.height * self.stride
    }

    /// Total bytes of the whole allocation — every buffer.
    #[must_use]
    pub const fn total_bytes(&self) -> usize {
        self.buffers * self.height * self.stride
    }
}
