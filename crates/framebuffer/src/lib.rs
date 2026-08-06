//! A text console over a linear (packed-pixel) framebuffer.
//!
//! On the QEMU `virt` machine the kernel prints over a PL011 UART. On a Raspberry
//! Pi 5 that path is gone before it starts: the UART on the GPIO header lives
//! behind the RP1 south-bridge on PCIe, so the *first* thing a kernel can show is
//! whatever the firmware already lit up — the HDMI framebuffer. This crate is the
//! portable half of that console: given a pointer to a framebuffer, its geometry,
//! and its pixel format, it draws an 8x8 font and behaves like a scrolling text
//! terminal. It knows nothing about VideoCore, mailboxes, or MMIO — the kernel
//! hands it the bytes.
//!
//! **Design for testing.** The interesting, bug-prone part is not the font shapes
//! (a wrong pixel there is merely ugly) but the *addressing*: bytes-per-pixel,
//! row pitch, channel shifts, scroll. So the renderer is written against a plain
//! `&mut [u8]` and every pixel-addressing rule is pinned by host tests using
//! synthetic glyphs whose exact output is known. The real font's visual accuracy
//! is confirmed later, live, on a screen — it cannot be unit-tested against a
//! reference we do not have.
#![cfg_attr(not(test), no_std)]

mod font;
pub use font::{glyph, GLYPH_HEIGHT, GLYPH_WIDTH};

/// How pixels are packed into memory, independent of geometry.
///
/// A framebuffer is `bytes_per_pixel` bytes per pixel, stored little-endian, with
/// each colour channel occupying `*_bits` bits starting at `*_shift`. This covers
/// the formats firmware actually hands over: 32-bit XRGB/XBGR and 16-bit RGB565.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelFormat {
    /// Bytes each pixel occupies in memory (2 or 4).
    pub bytes_per_pixel: u8,
    /// Bit position of the least-significant red bit.
    pub red_shift: u8,
    /// Number of red bits.
    pub red_bits: u8,
    /// Bit position of the least-significant green bit.
    pub green_shift: u8,
    /// Number of green bits.
    pub green_bits: u8,
    /// Bit position of the least-significant blue bit.
    pub blue_shift: u8,
    /// Number of blue bits.
    pub blue_bits: u8,
}

impl PixelFormat {
    /// 32-bit `0xAARRGGBB` little-endian (blue at byte 0). The most common thing a
    /// bootloader leaves configured; also what QEMU `ramfb` uses by default.
    #[must_use]
    pub const fn xrgb8888() -> Self {
        Self {
            bytes_per_pixel: 4,
            red_shift: 16,
            red_bits: 8,
            green_shift: 8,
            green_bits: 8,
            blue_shift: 0,
            blue_bits: 8,
        }
    }

    /// 32-bit `0xAABBGGRR` (red at byte 0). Some Pi firmware modes report BGR.
    #[must_use]
    pub const fn xbgr8888() -> Self {
        Self {
            bytes_per_pixel: 4,
            red_shift: 0,
            red_bits: 8,
            green_shift: 8,
            green_bits: 8,
            blue_shift: 16,
            blue_bits: 8,
        }
    }

    /// 16-bit `RRRRRGGGGGGBBBBB`. The fallback depth for small panels.
    #[must_use]
    pub const fn rgb565() -> Self {
        Self {
            bytes_per_pixel: 2,
            red_shift: 11,
            red_bits: 5,
            green_shift: 5,
            green_bits: 6,
            blue_shift: 0,
            blue_bits: 5,
        }
    }

    /// Pack 8-bit-per-channel RGB into this format's pixel word, quantising each
    /// channel down to the available bits. Returns the low `bytes_per_pixel`
    /// bytes' worth in a `u32`.
    #[must_use]
    pub const fn encode(&self, r: u8, g: u8, b: u8) -> u32 {
        let r = (r as u32) >> (8 - self.red_bits);
        let g = (g as u32) >> (8 - self.green_bits);
        let b = (b as u32) >> (8 - self.blue_bits);
        (r << self.red_shift) | (g << self.green_shift) | (b << self.blue_shift)
    }
}

/// A colour, kept as 8-bit-per-channel RGB and encoded per [`PixelFormat`] at
/// draw time so callers never think about the target's bit layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    /// Red channel, 0–255.
    pub r: u8,
    /// Green channel, 0–255.
    pub g: u8,
    /// Blue channel, 0–255.
    pub b: u8,
}

impl Rgb {
    /// A colour from its three channels.
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Black — the usual background.
    pub const BLACK: Rgb = Rgb::new(0, 0, 0);
    /// A light grey — a readable default foreground on black.
    pub const LIGHT_GREY: Rgb = Rgb::new(0xC0, 0xC0, 0xC0);
    /// A green, for the "we are alive" boot banner.
    pub const GREEN: Rgb = Rgb::new(0x33, 0xFF, 0x66);
    /// A red, for panics and errors.
    pub const RED: Rgb = Rgb::new(0xFF, 0x33, 0x33);
}

/// A raw framebuffer: a byte slice plus the geometry needed to address it.
///
/// `pitch` is the number of *bytes* between the start of one row and the next; it
/// is not necessarily `width * bytes_per_pixel`, because firmware often rounds the
/// stride up for alignment. Getting this wrong is the classic framebuffer bug —
/// the picture shears diagonally — which is exactly why it is a first-class field
/// and pinned by tests.
pub struct Framebuffer<'a> {
    buf: &'a mut [u8],
    width: usize,
    height: usize,
    pitch: usize,
    format: PixelFormat,
}

impl<'a> Framebuffer<'a> {
    /// Wrap a framebuffer. Returns `None` if the slice is too small for the
    /// claimed geometry, so a mis-parsed mode can never lead to an out-of-bounds
    /// write — every pixel access below is already bounds-checked, but this
    /// catches the mistake up front.
    #[must_use]
    pub fn new(
        buf: &'a mut [u8],
        width: usize,
        height: usize,
        pitch: usize,
        format: PixelFormat,
    ) -> Option<Self> {
        let bpp = format.bytes_per_pixel as usize;
        if bpp == 0 || pitch < width.checked_mul(bpp)? {
            return None;
        }
        // The last addressable byte is in the last row; require the slice to hold
        // at least up to the last pixel of the last row.
        let last_row = height.checked_sub(1)?.checked_mul(pitch)?;
        let need = last_row.checked_add(width.checked_mul(bpp)?)?;
        if buf.len() < need {
            return None;
        }
        Some(Self {
            buf,
            width,
            height,
            pitch,
            format,
        })
    }

    /// Width in pixels.
    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub fn height(&self) -> usize {
        self.height
    }

    /// Set one pixel. Out-of-range coordinates are ignored (never panic): the
    /// console clips at the edges rather than trusting its own arithmetic.
    pub fn put_pixel(&mut self, x: usize, y: usize, color: Rgb) {
        if x >= self.width || y >= self.height {
            return;
        }
        let bpp = self.format.bytes_per_pixel as usize;
        let off = y * self.pitch + x * bpp;
        let word = self.format.encode(color.r, color.g, color.b);
        let bytes = word.to_le_bytes();
        if let Some(dst) = self.buf.get_mut(off..off + bpp) {
            dst.copy_from_slice(&bytes[..bpp]);
        }
    }

    /// Fill the whole framebuffer with one colour.
    pub fn clear(&mut self, color: Rgb) {
        for y in 0..self.height {
            for x in 0..self.width {
                self.put_pixel(x, y, color);
            }
        }
    }

    /// Draw one 8x8 glyph with its top-left at pixel `(ox, oy)`. Set bits get
    /// `fg`; clear bits get `bg` (so a new glyph fully overwrites whatever was
    /// there — no ghosting when a line is redrawn).
    pub fn draw_glyph(&mut self, ox: usize, oy: usize, bits: &[u8; 8], fg: Rgb, bg: Rgb) {
        for (row, byte) in bits.iter().enumerate() {
            for col in 0..GLYPH_WIDTH {
                // Bit 0 is the leftmost pixel (matches the font's convention).
                let on = byte & (1 << col) != 0;
                self.put_pixel(ox + col, oy + row, if on { fg } else { bg });
            }
        }
    }

    /// Scroll the whole image up by `rows` pixels, filling the freed bottom band
    /// with `bg`. Uses `copy_within`, so it moves bytes without a scratch buffer —
    /// which matters in a kernel with no allocator on this path.
    pub fn scroll_up(&mut self, rows: usize, bg: Rgb) {
        if rows == 0 || rows >= self.height {
            self.clear(bg);
            return;
        }
        let shift = rows * self.pitch;
        let end = self.height * self.pitch;
        // Move rows [rows, height) up to [0, height-rows).
        self.buf.copy_within(shift..end, 0);
        // Clear the newly exposed rows at the bottom.
        for y in (self.height - rows)..self.height {
            for x in 0..self.width {
                self.put_pixel(x, y, bg);
            }
        }
    }
}

/// A scrolling text terminal on top of a [`Framebuffer`].
///
/// Tracks a cursor in character cells (each `GLYPH_WIDTH`×`GLYPH_HEIGHT` pixels),
/// handles the control bytes a boot log actually emits (`\n`, `\r`, `\t`,
/// backspace), wraps at the right edge and scrolls at the bottom. Bytes are UTF-8
/// from the caller; non-ASCII and unprintable bytes render as a visible box so
/// nothing silently vanishes on the one screen you have.
pub struct Console<'a> {
    fb: Framebuffer<'a>,
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
    fg: Rgb,
    bg: Rgb,
}

impl<'a> Console<'a> {
    /// Build a console over `fb`, compute the cell grid from its geometry, paint
    /// the background and home the cursor.
    pub fn new(mut fb: Framebuffer<'a>, fg: Rgb, bg: Rgb) -> Self {
        let cols = fb.width() / GLYPH_WIDTH;
        let rows = fb.height() / GLYPH_HEIGHT;
        fb.clear(bg);
        Self {
            fb,
            cols,
            rows,
            col: 0,
            row: 0,
            fg,
            bg,
        }
    }

    /// Columns (character cells across).
    #[must_use]
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Rows (character cells down).
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// The current cursor, in `(col, row)` cells — exposed for tests.
    #[must_use]
    pub fn cursor(&self) -> (usize, usize) {
        (self.col, self.row)
    }

    /// Set the text colour used for subsequently written glyphs.
    pub fn set_fg(&mut self, fg: Rgb) {
        self.fg = fg;
    }

    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 >= self.rows {
            self.fb.scroll_up(GLYPH_HEIGHT, self.bg);
        } else {
            self.row += 1;
        }
    }

    /// Write one byte, interpreting the control codes a console needs.
    pub fn write_byte(&mut self, byte: u8) {
        match byte {
            b'\n' => self.newline(),
            b'\r' => self.col = 0,
            b'\t' => {
                // Advance to the next multiple-of-8 column, wrapping if needed.
                let next = (self.col & !7) + 8;
                while self.col < next {
                    self.put_glyph(b' ');
                    if self.col == 0 {
                        break; // wrapped
                    }
                }
            }
            0x08 => {
                // Backspace: step left and erase, but not past the line start.
                if self.col > 0 {
                    self.col -= 1;
                    self.draw_cell(self.col, self.row, b' ');
                }
            }
            _ => self.put_glyph(byte),
        }
    }

    /// Write a whole string.
    pub fn write_str(&mut self, s: &str) {
        for byte in s.bytes() {
            self.write_byte(byte);
        }
    }

    fn draw_cell(&mut self, col: usize, row: usize, byte: u8) {
        let bits = glyph(byte);
        self.fb
            .draw_glyph(col * GLYPH_WIDTH, row * GLYPH_HEIGHT, &bits, self.fg, self.bg);
    }

    fn put_glyph(&mut self, byte: u8) {
        if self.cols == 0 || self.rows == 0 {
            return; // a framebuffer too small for even one cell
        }
        if self.col >= self.cols {
            self.newline();
        }
        self.draw_cell(self.col, self.row, byte);
        self.col += 1;
    }
}

impl core::fmt::Write for Console<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        Console::write_str(self, s);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- pixel format encoding ------------------------------------------------

    #[test]
    fn xrgb8888_channels_land_in_the_right_bytes() {
        let f = PixelFormat::xrgb8888();
        // Pure red -> 0x00FF0000; little-endian bytes = [00,00,FF,00].
        assert_eq!(f.encode(0xFF, 0, 0), 0x00FF_0000);
        assert_eq!(f.encode(0, 0xFF, 0), 0x0000_FF00);
        assert_eq!(f.encode(0, 0, 0xFF), 0x0000_00FF);
    }

    #[test]
    fn rgb565_quantises_and_packs() {
        let f = PixelFormat::rgb565();
        // Full white: 5+6+5 bits all set = 0xFFFF.
        assert_eq!(f.encode(0xFF, 0xFF, 0xFF), 0xFFFF);
        // Pure red occupies the top 5 bits.
        assert_eq!(f.encode(0xFF, 0, 0), 0xF800);
        assert_eq!(f.encode(0, 0xFF, 0), 0x07E0);
        assert_eq!(f.encode(0, 0, 0xFF), 0x001F);
    }

    // --- framebuffer addressing ----------------------------------------------

    fn fb32(w: usize, h: usize, pitch: usize) -> Vec<u8> {
        vec![0u8; h * pitch.max(w * 4)]
    }

    fn pixel32(buf: &[u8], pitch: usize, x: usize, y: usize) -> u32 {
        let off = y * pitch + x * 4;
        u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
    }

    #[test]
    fn put_pixel_uses_pitch_not_width() {
        // A pitch wider than width*bpp: writing (0,1) must land at byte `pitch`,
        // not at `width*4`. This is the shear bug the field exists to prevent.
        let width = 4;
        let height = 2;
        let pitch = 32; // deliberately > width*4 == 16
        let mut buf = fb32(width, height, pitch);
        let mut fb = Framebuffer::new(&mut buf, width, height, pitch, PixelFormat::xrgb8888())
            .expect("geometry fits");
        fb.put_pixel(0, 1, Rgb::new(0xFF, 0, 0));
        assert_eq!(pixel32(&buf, pitch, 0, 1), 0x00FF_0000);
        // The byte at width*4 (where a width-based bug would write) is untouched.
        assert_eq!(buf[width * 4], 0);
    }

    #[test]
    fn out_of_range_pixels_are_clipped_not_panicking() {
        let mut buf = fb32(4, 4, 16);
        let mut fb =
            Framebuffer::new(&mut buf, 4, 4, 16, PixelFormat::xrgb8888()).expect("fits");
        fb.put_pixel(99, 0, Rgb::new(0xFF, 0xFF, 0xFF));
        fb.put_pixel(0, 99, Rgb::new(0xFF, 0xFF, 0xFF));
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn new_rejects_too_small_buffer() {
        let mut buf = vec![0u8; 10];
        assert!(Framebuffer::new(&mut buf, 100, 100, 400, PixelFormat::xrgb8888()).is_none());
    }

    #[test]
    fn draw_glyph_maps_set_bits_to_fg_and_clear_to_bg() {
        // A synthetic glyph: only bit 0 of every row set -> a vertical bar down
        // the left column. This pins bit-order (bit 0 == leftmost) independent of
        // the real font.
        let bar = [0x01u8; 8];
        let width = 8;
        let height = 8;
        let pitch = width * 4;
        let mut buf = fb32(width, height, pitch);
        let fg = Rgb::new(0xFF, 0xFF, 0xFF);
        let bg = Rgb::BLACK;
        {
            let mut fb = Framebuffer::new(&mut buf, width, height, pitch, PixelFormat::xrgb8888())
                .unwrap();
            fb.draw_glyph(0, 0, &bar, fg, bg);
        }
        for y in 0..8 {
            assert_eq!(pixel32(&buf, pitch, 0, y), fg_word(), "col 0 row {y} should be fg");
            for x in 1..8 {
                assert_eq!(pixel32(&buf, pitch, x, y), 0, "col {x} row {y} should be bg");
            }
        }
    }

    fn fg_word() -> u32 {
        PixelFormat::xrgb8888().encode(0xFF, 0xFF, 0xFF)
    }

    #[test]
    fn real_glyph_bit_order_and_orientation_are_pinned() {
        // The synthetic-glyph test above pins bit-order against a hand-made bitmap;
        // this pins the *real font* on both axes at once, so a mirrored or flipped
        // font table (or a `1 << (7 - col)` bit-order slip) is caught. 'F' is
        // asymmetric both ways: its rows top-to-bottom are
        // 0x7F,0x46,0x16,0x1E,0x16,0x06,0x0F,0x00 — a full top bar and a left stem.
        let width = GLYPH_WIDTH;
        let height = GLYPH_HEIGHT;
        let pitch = width * 4;
        let mut buf = fb32(width, height, pitch);
        let white = Rgb::new(0xFF, 0xFF, 0xFF);
        {
            let mut fb =
                Framebuffer::new(&mut buf, width, height, pitch, PixelFormat::xrgb8888()).unwrap();
            fb.draw_glyph(0, 0, &glyph(b'F'), white, Rgb::BLACK);
        }
        let fg = fg_word();
        // Top row 0x7F: columns 0..=6 lit, column 7 clear. Row 0 being the bar pins
        // top-to-bottom order; column 0 being lit pins bit 0 == leftmost.
        for x in 0..7 {
            assert_eq!(pixel32(&buf, pitch, x, 0), fg, "top bar should cover col {x}");
        }
        assert_eq!(pixel32(&buf, pitch, 7, 0), 0, "top bar stops before col 7");
        // Row 6 is the short left stem 0x0F: columns 0..=3 lit, 4..=7 clear. A
        // vertically flipped glyph would put the full bar here instead.
        for x in 0..4 {
            assert_eq!(pixel32(&buf, pitch, x, 6), fg, "left stem should cover col {x}");
        }
        for x in 4..8 {
            assert_eq!(pixel32(&buf, pitch, x, 6), 0, "left stem is clear at col {x}");
        }
    }

    #[test]
    fn scroll_up_moves_rows_and_clears_the_bottom() {
        // 2 cells tall (16 px). Fill row-band 0 red, band 1 blue, scroll one glyph
        // height: band 1 moves to the top, bottom is cleared.
        let width = 8;
        let height = 16;
        let pitch = width * 4;
        let mut buf = fb32(width, height, pitch);
        let red = Rgb::new(0xFF, 0, 0);
        let blue = Rgb::new(0, 0, 0xFF);
        {
            let mut fb =
                Framebuffer::new(&mut buf, width, height, pitch, PixelFormat::xrgb8888()).unwrap();
            for y in 0..8 {
                for x in 0..8 {
                    fb.put_pixel(x, y, red);
                }
            }
            for y in 8..16 {
                for x in 0..8 {
                    fb.put_pixel(x, y, blue);
                }
            }
            fb.scroll_up(8, Rgb::BLACK);
        }
        let blue_word = PixelFormat::xrgb8888().encode(0, 0, 0xFF);
        // Top band is now blue.
        assert_eq!(pixel32(&buf, pitch, 0, 0), blue_word);
        assert_eq!(pixel32(&buf, pitch, 7, 7), blue_word);
        // Bottom band is cleared.
        assert_eq!(pixel32(&buf, pitch, 0, 8), 0);
        assert_eq!(pixel32(&buf, pitch, 7, 15), 0);
    }

    // --- console behaviour ----------------------------------------------------

    fn console_buf(cols: usize, rows: usize) -> (Vec<u8>, usize, usize, usize) {
        let width = cols * GLYPH_WIDTH;
        let height = rows * GLYPH_HEIGHT;
        let pitch = width * 4;
        (vec![0u8; height * pitch], width, height, pitch)
    }

    #[test]
    fn console_grid_is_geometry_over_glyph_size() {
        let (mut buf, w, h, pitch) = console_buf(10, 4);
        let fb = Framebuffer::new(&mut buf, w, h, pitch, PixelFormat::xrgb8888()).unwrap();
        let con = Console::new(fb, Rgb::LIGHT_GREY, Rgb::BLACK);
        assert_eq!(con.cols(), 10);
        assert_eq!(con.rows(), 4);
        assert_eq!(con.cursor(), (0, 0));
    }

    #[test]
    fn writing_advances_the_cursor_and_newline_returns_it() {
        let (mut buf, w, h, pitch) = console_buf(10, 4);
        let fb = Framebuffer::new(&mut buf, w, h, pitch, PixelFormat::xrgb8888()).unwrap();
        let mut con = Console::new(fb, Rgb::LIGHT_GREY, Rgb::BLACK);
        con.write_str("Hi");
        assert_eq!(con.cursor(), (2, 0));
        con.write_byte(b'\n');
        assert_eq!(con.cursor(), (0, 1));
    }

    #[test]
    fn a_printed_glyph_actually_sets_pixels() {
        // Falsifiable: 'A' is a non-blank glyph, so *some* pixel in cell (0,0)
        // must be foreground after printing it. A no-op console would leave black.
        let (mut buf, w, h, pitch) = console_buf(4, 2);
        {
            let fb = Framebuffer::new(&mut buf, w, h, pitch, PixelFormat::xrgb8888()).unwrap();
            let mut con = Console::new(fb, Rgb::new(0xFF, 0xFF, 0xFF), Rgb::BLACK);
            con.write_byte(b'A');
        }
        let mut any = false;
        for y in 0..GLYPH_HEIGHT {
            for x in 0..GLYPH_WIDTH {
                if pixel32(&buf, pitch, x, y) != 0 {
                    any = true;
                }
            }
        }
        assert!(any, "printing 'A' set no pixels");
    }

    #[test]
    fn space_is_blank_but_a_letter_is_not() {
        assert_eq!(glyph(b' '), [0u8; 8]);
        assert_ne!(glyph(b'A'), [0u8; 8]);
    }

    #[test]
    fn wrapping_at_the_right_edge_moves_to_the_next_row() {
        let (mut buf, w, h, pitch) = console_buf(3, 4);
        let fb = Framebuffer::new(&mut buf, w, h, pitch, PixelFormat::xrgb8888()).unwrap();
        let mut con = Console::new(fb, Rgb::LIGHT_GREY, Rgb::BLACK);
        con.write_str("abcd"); // 4 chars into a 3-wide console
        assert_eq!(con.cursor(), (1, 1));
    }

    #[test]
    fn carriage_return_homes_the_column_without_changing_row() {
        let (mut buf, w, h, pitch) = console_buf(10, 4);
        let fb = Framebuffer::new(&mut buf, w, h, pitch, PixelFormat::xrgb8888()).unwrap();
        let mut con = Console::new(fb, Rgb::LIGHT_GREY, Rgb::BLACK);
        con.write_str("abc\r");
        assert_eq!(con.cursor(), (0, 0), "\\r returns to column 0 on the same row");
    }

    #[test]
    fn tab_advances_to_the_next_multiple_of_eight() {
        let (mut buf, w, h, pitch) = console_buf(20, 2);
        let fb = Framebuffer::new(&mut buf, w, h, pitch, PixelFormat::xrgb8888()).unwrap();
        let mut con = Console::new(fb, Rgb::LIGHT_GREY, Rgb::BLACK);
        con.write_str("ab\t"); // column 2, then a tab lands on column 8
        assert_eq!(con.cursor(), (8, 0));
    }

    #[test]
    fn backspace_steps_left_but_not_past_the_line_start() {
        let (mut buf, w, h, pitch) = console_buf(10, 2);
        let fb = Framebuffer::new(&mut buf, w, h, pitch, PixelFormat::xrgb8888()).unwrap();
        let mut con = Console::new(fb, Rgb::LIGHT_GREY, Rgb::BLACK);
        con.write_str("ab\x08"); // column 2 -> backspace -> column 1
        assert_eq!(con.cursor(), (1, 0));
        con.write_byte(0x08);
        con.write_byte(0x08); // already at column 0: stays put, never underflows
        assert_eq!(con.cursor(), (0, 0));
    }

    #[test]
    fn scrolling_pins_the_cursor_at_the_last_row() {
        let (mut buf, w, h, pitch) = console_buf(4, 2);
        let fb = Framebuffer::new(&mut buf, w, h, pitch, PixelFormat::xrgb8888()).unwrap();
        let mut con = Console::new(fb, Rgb::LIGHT_GREY, Rgb::BLACK);
        con.write_str("a\nb\nc"); // three lines into a 2-row console
        let (_, row) = con.cursor();
        assert_eq!(row, 1, "cursor should stay on the last row while scrolling");
    }
}
