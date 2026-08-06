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
}
