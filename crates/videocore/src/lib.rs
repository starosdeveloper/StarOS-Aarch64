//! Raspberry Pi VideoCore **property mailbox** messages.
//!
//! On a Raspberry Pi the ARM cores ask the VideoCore GPU to do things — allocate a
//! framebuffer, report clock rates, read the board serial — by handing it a
//! *property-tag* message through a mailbox. The mailbox itself is four MMIO
//! registers and a doorbell (that part is board `unsafe` and lives in the kernel's
//! arch layer); the *message* is a precisely-laid-out block of 32-bit words, and
//! getting that layout wrong is the classic silent failure — the GPU returns a
//! zero base or the wrong pitch and the screen stays black with nothing to debug.
//!
//! So, exactly as with [`staros_fdt`](../staros_fdt/index.html) and the fw_cfg/ramfb
//! split, this crate is the *pure, host-tested* half: it builds the request buffer
//! and parses the reply as ordinary code with exact-value tests, and knows nothing
//! about MMIO. The kernel fills a 16-byte-aligned physical buffer with
//! [`build_fb_message`], rings the doorbell on channel [`CHANNEL_PROP`], and reads
//! the result back with [`parse_fb_response`].
//!
//! ## The property message format
//! A message is a contiguous `[u32]`:
//! ```text
//!   [0] total size in bytes (whole buffer, including this word and padding)
//!   [1] request/response code: 0 on request, 0x8000_0000 = success on reply
//!   then a sequence of tags, each:
//!     tag id
//!     value buffer size in bytes
//!     request/response code: 0 on request; on reply bit 31 set + low bits = the
//!                            number of bytes the GPU wrote into the value buffer
//!     value buffer  (`ceil(value_size/4)` words; holds the request, then is
//!                    overwritten in place by the reply)
//!   [n] end tag = 0
//!   padding to a 16-byte boundary
//! ```
//! The value buffer is sized to the *larger* of what the request and the reply
//! need, so the GPU can overwrite the request in place — e.g. `ALLOCATE_BUFFER`
//! sends `[alignment, 0]` and gets back `[base, size]`.
#![cfg_attr(not(test), no_std)]

/// Mailbox channel for the property-tags ARM→VC interface.
pub const CHANNEL_PROP: u32 = 8;

/// Overall request/response code words.
const CODE_REQUEST: u32 = 0x0000_0000;
/// Success reply code (in word `[1]` of a processed buffer).
pub const CODE_SUCCESS: u32 = 0x8000_0000;

// Property tag ids (a small subset — the framebuffer set).
/// Set the physical (display) width/height. Value `[w, h]`.
pub const TAG_SET_PHYSICAL_WH: u32 = 0x0004_8003;
/// Set the virtual (buffer) width/height. Value `[w, h]`.
pub const TAG_SET_VIRTUAL_WH: u32 = 0x0004_8004;
/// Set the virtual offset (panning). Value `[x, y]`.
pub const TAG_SET_VIRTUAL_OFFSET: u32 = 0x0004_8009;
/// Set bits per pixel. Value `[depth]`.
pub const TAG_SET_DEPTH: u32 = 0x0004_8005;
/// Set channel order. Value `[order]` (see [`PIXEL_ORDER_RGB`]).
pub const TAG_SET_PIXEL_ORDER: u32 = 0x0004_8006;
/// Allocate the framebuffer. Request `[alignment, 0]`, reply `[base, size]`.
pub const TAG_ALLOCATE_BUFFER: u32 = 0x0004_0001;
/// Get bytes per row. Reply `[pitch]`.
pub const TAG_GET_PITCH: u32 = 0x0004_0008;
/// The terminating tag.
pub const TAG_END: u32 = 0x0000_0000;

/// `SET_PIXEL_ORDER` value: blue in the low byte.
pub const PIXEL_ORDER_BGR: u32 = 0;
/// `SET_PIXEL_ORDER` value: red in the low byte.
pub const PIXEL_ORDER_RGB: u32 = 1;

/// Bit 31 marks a tag/overall code word as a *reply*.
const RESPONSE_BIT: u32 = 0x8000_0000;

/// Number of `u32` words the framebuffer message occupies, including the 16-byte
/// padding. The framebuffer request is fixed, so this is a constant the caller can
/// use to size the physical scratch buffer.
pub const FB_MSG_WORDS: usize = 36;

/// A request to allocate and configure a framebuffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FbRequest {
    /// Width in pixels (physical == virtual; no panning).
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bits per pixel (typically 32).
    pub depth: u32,
    /// Channel order: [`PIXEL_ORDER_RGB`] or [`PIXEL_ORDER_BGR`].
    pub pixel_order: u32,
    /// How many full-screen buffers to make room for, stacked vertically.
    ///
    /// This is the whole of double buffering on a Pi, and it is spelled as a
    /// *taller virtual framebuffer* rather than as a second allocation because that
    /// is what the firmware offers. `SET_PHYSICAL_WH` is what the display scans;
    /// `SET_VIRTUAL_WH` is how big the buffer behind it is; and
    /// [`build_offset_message`] moves the window between them. So two buffers is
    /// one allocation of `height * 2` rows, with the second screen starting at
    /// y = `height`.
    ///
    /// One is the old behaviour exactly: virtual equals physical and there is
    /// nowhere to pan to.
    pub buffers: u32,
}

/// What the GPU granted in reply to an [`FbRequest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FbAllocation {
    /// GPU *bus* address of the pixel buffer. Convert to an ARM physical address
    /// with [`bus_to_phys`] before mapping it — the two are not the same on a Pi.
    pub bus_base: u32,
    /// Size of the pixel buffer in bytes.
    pub size: u32,
    /// Bytes per row. May exceed `width * depth/8`, so the console must honour it.
    pub pitch: u32,
    /// Width the GPU actually configured (it may clamp the request).
    pub width: u32,
    /// Height the GPU actually configured.
    pub height: u32,
}

/// Append a tag to `buf` at word cursor `at`: id, value size in bytes, request
/// code, then `values` (already the full value-buffer width in words). Returns the
/// new cursor. Used only by [`build_fb_message`], where `buf` is provably large
/// enough, so an overrun is a bug in this crate, not reachable input.
fn put_tag(buf: &mut [u32], at: usize, id: u32, value_words: &[u32]) -> usize {
    buf[at] = id;
    buf[at + 1] = (value_words.len() * 4) as u32; // value buffer size in bytes
    buf[at + 2] = CODE_REQUEST;
    let start = at + 3;
    buf[start..start + value_words.len()].copy_from_slice(value_words);
    start + value_words.len()
}

/// Build the framebuffer property message for `req` into a fixed buffer, returning
/// it alongside its byte length (word `[0]`). The caller copies these
/// [`FB_MSG_WORDS`] words into a 16-byte-aligned physical buffer and rings the
/// doorbell; the GPU overwrites the value buffers in place.
#[must_use]
pub fn build_fb_message(req: &FbRequest) -> ([u32; FB_MSG_WORDS], usize) {
    let mut buf = [0u32; FB_MSG_WORDS];

    // Word [0] is the total size, filled in once we know the used length; [1] is
    // the request code.
    buf[1] = CODE_REQUEST;
    let mut at = 2;

    at = put_tag(&mut buf, at, TAG_SET_PHYSICAL_WH, &[req.width, req.height]);
    // Virtual height is physical height times the number of buffers: the display
    // scans `height` rows, the allocation holds `height * buffers` of them, and the
    // offset below chooses which screenful is on show. `buffers` of one leaves this
    // exactly as it was.
    at = put_tag(
        &mut buf,
        at,
        TAG_SET_VIRTUAL_WH,
        &[req.width, req.height * req.buffers.max(1)],
    );
    at = put_tag(&mut buf, at, TAG_SET_VIRTUAL_OFFSET, &[0, 0]);
    at = put_tag(&mut buf, at, TAG_SET_DEPTH, &[req.depth]);
    at = put_tag(&mut buf, at, TAG_SET_PIXEL_ORDER, &[req.pixel_order]);
    // ALLOCATE_BUFFER: request is [alignment, 0]; reply overwrites with [base, size].
    at = put_tag(&mut buf, at, TAG_ALLOCATE_BUFFER, &[PAGE_ALIGN, 0]);
    // GET_PITCH: request has no input; reply writes [pitch]. One word of value.
    at = put_tag(&mut buf, at, TAG_GET_PITCH, &[0]);

    buf[at] = TAG_END;
    at += 1;

    // Total size = used words rounded up to a 16-byte (4-word) boundary, in bytes.
    let words = (at + 3) & !3;
    buf[0] = (words * 4) as u32;
    (buf, words * 4)
}

/// Framebuffer alignment requested of `ALLOCATE_BUFFER` (one page).
const PAGE_ALIGN: u32 = 4096;

/// A walker over the tags of a (request or reply) property buffer.
///
/// Never panics on malformed input: a truncated or self-overrunning buffer just
/// ends iteration — the reply comes from firmware and is treated as untrusted.
pub struct Tags<'a> {
    buf: &'a [u32],
    /// Word cursor, starting past the two header words.
    at: usize,
}

/// One tag yielded by [`Tags`]: its id, the reply code word, and its value words.
#[derive(Clone, Copy, Debug)]
pub struct Tag<'a> {
    /// Tag id.
    pub id: u32,
    /// The tag's third word: on a reply, bit 31 is set and the low bits are the
    /// number of *bytes* the GPU wrote.
    pub code: u32,
    /// The value buffer (full width, in words).
    pub values: &'a [u32],
}

impl<'a> Tags<'a> {
    /// Walk the tags of `buf`, a whole property buffer (size word, code word,
    /// tags, end tag).
    #[must_use]
    pub fn new(buf: &'a [u32]) -> Self {
        Self {
            buf,
            at: 2.min(buf.len()),
        }
    }

    /// The buffer's overall reply code (word `[1]`), or `None` if absent.
    #[must_use]
    pub fn overall_code(buf: &'a [u32]) -> Option<u32> {
        buf.get(1).copied()
    }
}

impl<'a> Iterator for Tags<'a> {
    type Item = Tag<'a>;

    fn next(&mut self) -> Option<Tag<'a>> {
        // Need id + size + code.
        let id = *self.buf.get(self.at)?;
        if id == TAG_END {
            return None;
        }
        let value_bytes = *self.buf.get(self.at + 1)? as usize;
        let code = *self.buf.get(self.at + 2)?;
        let value_words = value_bytes / 4;
        let start = self.at + 3;
        let end = start.checked_add(value_words)?;
        let values = self.buf.get(start..end)?;
        self.at = end;
        Some(Tag { id, code, values })
    }
}

/// Read the reply value words of the first tag with id `id` in a processed buffer.
fn tag_values(buf: &[u32], id: u32) -> Option<&[u32]> {
    Tags::new(buf).find(|t| t.id == id).map(|t| t.values)
}

/// Parse a processed framebuffer buffer (the same words [`build_fb_message`]
/// produced, after the GPU wrote its replies) into an [`FbAllocation`].
///
/// Returns `None` unless the overall success bit is set and every framebuffer tag
/// we need is present with a non-zero allocation — a black-screen firmware failure
/// (zero base) is reported as `None`, not a bogus mapping.
#[must_use]
pub fn parse_fb_response(buf: &[u32]) -> Option<FbAllocation> {
    if Tags::overall_code(buf)? & RESPONSE_BIT == 0 {
        return None;
    }
    let alloc = tag_values(buf, TAG_ALLOCATE_BUFFER)?;
    let (bus_base, size) = (*alloc.first()?, *alloc.get(1)?);
    let pitch = *tag_values(buf, TAG_GET_PITCH)?.first()?;
    let wh = tag_values(buf, TAG_SET_PHYSICAL_WH)?;
    let (width, height) = (*wh.first()?, *wh.get(1)?);

    if bus_base == 0 || size == 0 || pitch == 0 || width == 0 || height == 0 {
        return None;
    }
    Some(FbAllocation {
        bus_base,
        size,
        pitch,
        width,
        height,
    })
}

/// Number of `u32` words a [`build_offset_message`] buffer occupies, padding
/// included.
pub const OFFSET_MSG_WORDS: usize = 8;

/// Build a `SET_VIRTUAL_OFFSET` message: move the scanned-out window to
/// `(x, y)` within the virtual framebuffer.
///
/// The flip, on a Pi. There is no address here and there could not be one — the
/// buffer was allocated once, and what moves is which of its rows the display
/// controller starts at. A caller with `buffers` screens stacked passes
/// `y = index * height`.
///
/// Its own message and not a re-send of [`build_fb_message`] with a different
/// offset, because that one carries `ALLOCATE_BUFFER`: re-sending it would ask the
/// firmware to allocate again, on every frame, and the base could move under a
/// mapping the kernel has already handed to a process.
#[must_use]
pub fn build_offset_message(x: u32, y: u32) -> ([u32; OFFSET_MSG_WORDS], usize) {
    let mut buf = [0u32; OFFSET_MSG_WORDS];
    buf[1] = CODE_REQUEST;
    let mut at = 2;
    at = put_tag(&mut buf, at, TAG_SET_VIRTUAL_OFFSET, &[x, y]);
    buf[at] = TAG_END;
    at += 1;
    let words = (at + 3) & !3;
    buf[0] = (words * 4) as u32;
    (buf, words * 4)
}

/// The `(x, y)` the firmware says the window is now at, from a processed
/// [`build_offset_message`] buffer.
///
/// `None` unless the overall success bit is set and the tag came back — and the
/// values are the firmware's, not the ones that were asked for. A Pi clamps an
/// offset that would run past the virtual height, and a flip that was silently
/// clamped to zero is a compositor drawing into a buffer nobody is looking at.
/// Reading back what actually happened is the only way that failure is visible.
#[must_use]
pub fn parse_offset_response(buf: &[u32]) -> Option<(u32, u32)> {
    if Tags::overall_code(buf)? & RESPONSE_BIT == 0 {
        return None;
    }
    let v = tag_values(buf, TAG_SET_VIRTUAL_OFFSET)?;
    Some((*v.first()?, *v.get(1)?))
}

/// Convert a VideoCore *bus* address to an ARM *physical* address.
///
/// The GPU hands back framebuffer addresses in its own address space, where RAM is
/// aliased through the top bits to select the L2 cache behaviour (`0xC000_0000` =
/// uncached alias on BCM2835/2711). Clearing those alias bits gives the address the
/// ARM MMU actually uses. This is the legacy mask that holds on Pi 1–4; a board
/// whose alias scheme differs overrides it in the arch layer.
#[must_use]
pub const fn bus_to_phys(bus: u32) -> u32 {
    bus & 0x3FFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_req() -> FbRequest {
        FbRequest {
            width: 640,
            height: 480,
            depth: 32,
            pixel_order: PIXEL_ORDER_RGB,
            buffers: 2,
        }
    }

    #[test]
    fn message_header_and_size_are_well_formed() {
        let (buf, len) = build_fb_message(&sample_req());
        // Byte length is the size word, a multiple of 16, and fits the buffer.
        assert_eq!(len as u32, buf[0]);
        assert_eq!(len % 16, 0);
        assert!(len / 4 <= FB_MSG_WORDS);
        assert_eq!(buf[1], CODE_REQUEST);
        // The last used word is the end tag.
        assert_eq!(buf[len / 4 - 1], TAG_END);
    }

    #[test]
    fn request_carries_the_geometry_in_the_right_tags() {
        let (buf, _) = build_fb_message(&sample_req());
        assert_eq!(tag_values(&buf, TAG_SET_PHYSICAL_WH), Some(&[640, 480][..]));
        // Virtual is two screens tall: what the display scans is 480 rows, what the
        // allocation holds is 960, and the second screen begins at y = 480.
        assert_eq!(tag_values(&buf, TAG_SET_VIRTUAL_WH), Some(&[640, 960][..]));
        assert_eq!(tag_values(&buf, TAG_SET_DEPTH), Some(&[32][..]));
        assert_eq!(
            tag_values(&buf, TAG_SET_PIXEL_ORDER),
            Some(&[PIXEL_ORDER_RGB][..])
        );
        // ALLOCATE_BUFFER sends [alignment, 0].
        assert_eq!(
            tag_values(&buf, TAG_ALLOCATE_BUFFER),
            Some(&[PAGE_ALIGN, 0][..])
        );
    }

    /// Emulate the GPU: set the success bit and overwrite the value buffers of the
    /// allocate/pitch/physical-wh tags in place, exactly as firmware would.
    fn fake_firmware_reply(buf: &mut [u32], bus_base: u32, size: u32, pitch: u32, w: u32, h: u32) {
        buf[1] = CODE_SUCCESS;
        // Re-walk and patch by locating each tag's value start.
        let mut at = 2;
        while at < buf.len() {
            let id = buf[at];
            if id == TAG_END {
                break;
            }
            let vbytes = buf[at + 1] as usize;
            let vwords = vbytes / 4;
            let start = at + 3;
            buf[at + 2] = RESPONSE_BIT | (vbytes as u32);
            match id {
                TAG_ALLOCATE_BUFFER => {
                    buf[start] = bus_base;
                    buf[start + 1] = size;
                }
                TAG_GET_PITCH => buf[start] = pitch,
                TAG_SET_PHYSICAL_WH => {
                    buf[start] = w;
                    buf[start + 1] = h;
                }
                _ => {}
            }
            at = start + vwords;
        }
    }

    #[test]
    fn parses_a_successful_reply() {
        let (mut buf, _) = build_fb_message(&sample_req());
        fake_firmware_reply(&mut buf, 0xDEAD_0000, 640 * 480 * 4, 640 * 4, 640, 480);
        let a = parse_fb_response(&buf).expect("should parse");
        assert_eq!(a.bus_base, 0xDEAD_0000);
        assert_eq!(a.size, 640 * 480 * 4);
        assert_eq!(a.pitch, 640 * 4);
        assert_eq!(a.width, 640);
        assert_eq!(a.height, 480);
    }

    #[test]
    fn honours_a_pitch_wider_than_width() {
        // Firmware often rounds the pitch up; the parser must report it verbatim.
        let (mut buf, _) = build_fb_message(&sample_req());
        fake_firmware_reply(&mut buf, 0x3000_0000, 800 * 480 * 4, 800 * 4, 640, 480);
        let a = parse_fb_response(&buf).unwrap();
        assert_eq!(a.pitch, 800 * 4);
        assert!(a.pitch as usize > a.width as usize * 4);
    }

    #[test]
    fn no_success_bit_is_rejected() {
        let (mut buf, _) = build_fb_message(&sample_req());
        fake_firmware_reply(&mut buf, 0x3000_0000, 100, 100, 640, 480);
        buf[1] = CODE_REQUEST; // clear the success bit the GPU would have set
        assert_eq!(parse_fb_response(&buf), None);
    }

    #[test]
    fn zero_base_is_rejected_not_mapped() {
        let (mut buf, _) = build_fb_message(&sample_req());
        // Success bit set but the GPU could not allocate: base 0.
        fake_firmware_reply(&mut buf, 0, 0, 640 * 4, 640, 480);
        assert_eq!(parse_fb_response(&buf), None);
    }

    #[test]
    fn bus_to_phys_clears_the_cache_alias() {
        assert_eq!(bus_to_phys(0xDEAD_0000), 0x1EAD_0000);
        assert_eq!(bus_to_phys(0x3E00_0000), 0x3E00_0000); // already physical
        assert_eq!(bus_to_phys(0xFFFF_FFFF), 0x3FFF_FFFF);
    }

    #[test]
    fn tag_walker_never_panics_on_garbage() {
        // A buffer whose value sizes overrun must terminate iteration, not panic.
        let mut buf = [0u32; 8];
        buf[0] = 32;
        buf[1] = CODE_SUCCESS;
        buf[2] = TAG_ALLOCATE_BUFFER;
        buf[3] = 0xFFFF_FFFF; // absurd value size
        buf[4] = RESPONSE_BIT;
        // Must not panic, and must find nothing usable.
        assert_eq!(parse_fb_response(&buf), None);
        assert_eq!(Tags::new(&buf).count(), 0);
    }

    #[test]
    fn single_byte_corruption_sweep_never_panics() {
        let (base, _) = build_fb_message(&sample_req());
        for w in 0..FB_MSG_WORDS {
            for bit in 0..32 {
                let mut buf = base;
                buf[w] ^= 1 << bit;
                // Neither walking nor parsing may panic on any single-bit flip.
                let _ = parse_fb_response(&buf);
                let _ = Tags::new(&buf).count();
            }
        }
    }

    #[test]
    fn one_buffer_asks_for_exactly_the_old_message() {
        // The claim that this change is opt-in: with one buffer the virtual size
        // equals the physical one, which is byte for byte what was sent before.
        let mut req = sample_req();
        req.buffers = 1;
        let (buf, _) = build_fb_message(&req);
        assert_eq!(tag_values(&buf, TAG_SET_VIRTUAL_WH), Some(&[640, 480][..]));
        // And zero is treated as one rather than as a zero-height allocation,
        // which the firmware would answer with a black screen and no error.
        req.buffers = 0;
        let (buf, _) = build_fb_message(&req);
        assert_eq!(tag_values(&buf, TAG_SET_VIRTUAL_WH), Some(&[640, 480][..]));
    }

    #[test]
    fn the_offset_message_is_well_formed_and_carries_the_pan() {
        let (buf, len) = build_offset_message(0, 480);
        assert_eq!(len as u32, buf[0]);
        assert_eq!(len % 16, 0);
        assert!(len / 4 <= OFFSET_MSG_WORDS);
        assert_eq!(buf[1], CODE_REQUEST);
        assert_eq!(buf[len / 4 - 1], TAG_END);
        assert_eq!(tag_values(&buf, TAG_SET_VIRTUAL_OFFSET), Some(&[0, 480][..]));
        // It must not carry ALLOCATE_BUFFER: re-sending that on every flip would
        // ask the firmware to allocate again, and the base could move under a
        // mapping the kernel has already handed to a process.
        assert_eq!(tag_values(&buf, TAG_ALLOCATE_BUFFER), None);
    }

    #[test]
    fn the_offset_reply_is_the_firmware_s_answer_not_the_request() {
        let (mut buf, _) = build_offset_message(0, 480);
        // A firmware that clamped the pan to zero — the failure this parse exists
        // to make visible, since the request would read back as 480 and the screen
        // would be showing the buffer nobody is drawing into.
        buf[1] = CODE_SUCCESS;
        buf[4] = RESPONSE_BIT | 8;
        buf[5] = 0;
        buf[6] = 0;
        assert_eq!(parse_offset_response(&buf), Some((0, 0)));
    }

    #[test]
    fn an_offset_reply_without_the_success_bit_is_none() {
        let (buf, _) = build_offset_message(0, 480);
        // Straight out of the builder the overall code is a request, not a reply.
        assert_eq!(parse_offset_response(&buf), None);
    }

    #[test]
    fn offset_corruption_sweep_never_panics() {
        let (base, _) = build_offset_message(0, 480);
        for w in 0..OFFSET_MSG_WORDS {
            for bit in 0..32 {
                let mut buf = base;
                buf[w] ^= 1 << bit;
                let _ = parse_offset_response(&buf);
                let _ = Tags::new(&buf).count();
            }
        }
    }
}
