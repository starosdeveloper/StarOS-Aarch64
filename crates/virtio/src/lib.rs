//! Virtio: the split virtqueue's layout, the MMIO register map, and the input
//! event format — as arithmetic, with no MMIO anywhere in sight.
//!
//! A virtqueue is three arrays in one buffer that a device and a driver both
//! read. Almost everything that can go wrong with one is a *layout* mistake:
//! a ring placed at the wrong offset, an index not wrapped, a length in the wrong
//! unit. None of those produce an error — the device simply never answers, or
//! answers with rubbish, and there is nothing to read afterwards that says why.
//! That is exactly the shape of bug the pure-crate discipline in this tree exists
//! for, so the layout lives here with exact-value tests and the driver is left
//! with the doorbell.
//!
//! ## What the split virtqueue actually is
//! Three regions, in this order, in one physically-contiguous buffer:
//!
//! | Region | Size | Written by |
//! |---|---|---|
//! | descriptor table | `16 * size` | driver |
//! | available ring | `6 + 2 * size` | driver |
//! | used ring | `6 + 8 * size` | **device** |
//!
//! The used ring must start on a 4096-byte boundary in the legacy MMIO layout
//! this targets, which is why [`QueueLayout`] exists rather than three `const`
//! offsets: the padding between the second and third region depends on the queue
//! size, and getting it wrong points the device's writes into the driver's ring.
//!
//! ## What is deliberately not here
//! Feature negotiation beyond the status handshake, indirect descriptors, packed
//! virtqueues, and anything to do with PCI. The one device this needs to drive is
//! `virtio-input` over MMIO, which uses two queues of a handful of descriptors and
//! no features at all.

#![cfg_attr(not(test), no_std)]

/// MMIO register offsets from a virtio-mmio device's base address.
///
/// The values are the transport's, not ours; they are collected here so a driver
/// reads `reg::QUEUE_NUM` rather than `0x38`, and so a typo is a compile error
/// rather than a device that ignores its configuration.
pub mod reg {
    /// `'virt'` little-endian — the first thing to check, and the cheapest way to
    /// tell a real transport from an empty slot in a device tree.
    pub const MAGIC: usize = 0x000;
    /// Transport version: 1 = legacy, 2 = modern.
    pub const VERSION: usize = 0x004;
    /// Which kind of device this is; 0 means the slot is empty.
    pub const DEVICE_ID: usize = 0x008;
    /// Vendor identifier.
    pub const VENDOR_ID: usize = 0x00c;
    /// Feature bits the device offers (selected by [`DEVICE_FEATURES_SEL`]).
    pub const DEVICE_FEATURES: usize = 0x010;
    /// Which 32-bit word of the device's features to read.
    pub const DEVICE_FEATURES_SEL: usize = 0x014;
    /// Feature bits the driver accepts.
    pub const DRIVER_FEATURES: usize = 0x020;
    /// Which 32-bit word of the driver's features to write.
    pub const DRIVER_FEATURES_SEL: usize = 0x024;
    /// Legacy only: the guest page size the PFN below is expressed in.
    pub const GUEST_PAGE_SIZE: usize = 0x028;
    /// Which queue the queue registers refer to.
    pub const QUEUE_SEL: usize = 0x030;
    /// The largest queue size this device supports; 0 means the queue is unusable.
    pub const QUEUE_NUM_MAX: usize = 0x034;
    /// The queue size the driver chose.
    pub const QUEUE_NUM: usize = 0x038;
    /// Legacy only: alignment of the used ring, in bytes.
    pub const QUEUE_ALIGN: usize = 0x03c;
    /// Legacy only: the queue's buffer, as a page-frame number.
    pub const QUEUE_PFN: usize = 0x040;
    /// Ring the device when a queue has new buffers: write the queue index.
    pub const QUEUE_NOTIFY: usize = 0x050;
    /// Why the device interrupted; write the same bits to [`INTERRUPT_ACK`].
    pub const INTERRUPT_STATUS: usize = 0x060;
    /// Acknowledge the interrupt bits read above.
    pub const INTERRUPT_ACK: usize = 0x064;
    /// The device-status handshake (see [`status`]).
    pub const STATUS: usize = 0x070;
    /// Device-specific configuration space begins here.
    pub const CONFIG: usize = 0x100;
}

/// Device-status bits, written to [`reg::STATUS`] in this order. The device is
/// entitled to ignore everything until it has seen them.
pub mod status {
    /// The driver has noticed the device.
    pub const ACKNOWLEDGE: u32 = 1;
    /// The driver knows how to drive it.
    pub const DRIVER: u32 = 2;
    /// The driver is ready; queues are configured.
    pub const DRIVER_OK: u32 = 4;
    /// Feature negotiation is complete (modern transports only).
    pub const FEATURES_OK: u32 = 8;
    /// The driver has given up on this device.
    pub const FAILED: u32 = 0x80;
}

/// `'virt'` as the magic register reads it.
pub const MAGIC_VALUE: u32 = 0x7472_6976;

/// The virtio device id of an input device (keyboard, tablet, mouse).
pub const DEVICE_ID_INPUT: u32 = 18;

/// Descriptor flag: the buffer continues in `next`.
pub const DESC_F_NEXT: u16 = 1;
/// Descriptor flag: the **device** writes this buffer. Every buffer an input
/// device fills carries it; forgetting it is the classic way to get a queue that
/// the device politely declines to use.
pub const DESC_F_WRITE: u16 = 2;

/// One descriptor: where a buffer is, how big, and whether the device may write
/// it. Sixteen bytes, in this order, little-endian — the device reads this memory
/// directly, so the layout is the contract.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Descriptor {
    /// Physical address of the buffer, as the *device* addresses memory.
    pub addr: u64,
    /// Length in bytes.
    pub len: u32,
    /// [`DESC_F_NEXT`] / [`DESC_F_WRITE`].
    pub flags: u16,
    /// Index of the next descriptor, when [`DESC_F_NEXT`] is set.
    pub next: u16,
}

/// Size of one descriptor in the table.
pub const DESC_BYTES: usize = 16;

/// Where each of a queue's three regions begins, for a given queue size.
///
/// Sizes and offsets rather than pointers: this crate never touches memory. The
/// driver adds these to whatever address it allocated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueLayout {
    /// Number of descriptors. Always a power of two.
    size: u16,
    /// Byte offset of the available ring from the start of the buffer.
    avail: usize,
    /// Byte offset of the used ring, aligned as the transport requires.
    used: usize,
    /// Total bytes the queue occupies.
    total: usize,
}

impl QueueLayout {
    /// Compute the layout for `size` descriptors with the used ring aligned to
    /// `align` bytes.
    ///
    /// Returns `None` for a size that is zero, not a power of two, or larger than
    /// 32768 — all three are outside what the specification allows, and each would
    /// otherwise produce a queue the device silently refuses.
    #[must_use]
    pub const fn new(size: u16, align: usize) -> Option<Self> {
        if size == 0 || size > 32768 || !size.is_power_of_two() || align == 0 {
            return None;
        }
        if !align.is_power_of_two() {
            return None;
        }
        let n = size as usize;
        // Descriptor table, then the available ring immediately after it.
        let avail = n * DESC_BYTES;
        // flags (2) + idx (2) + ring (2 * n) + used_event (2)
        let avail_end = avail + 6 + 2 * n;
        // The used ring starts on the next `align` boundary. This padding is the
        // whole reason this type exists: it depends on the queue size, and a
        // driver that assumes a fixed offset points the device's writes into its
        // own available ring.
        let used = (avail_end + align - 1) & !(align - 1);
        // flags (2) + idx (2) + ring (8 * n) + avail_event (2)
        let total = used + 6 + 8 * n;
        Some(Self {
            size,
            avail,
            used,
            total,
        })
    }

    /// Number of descriptors.
    #[must_use]
    pub const fn size(&self) -> u16 {
        self.size
    }

    /// Byte offset of the descriptor table (always zero, named for symmetry).
    #[must_use]
    pub const fn desc_offset(&self) -> usize {
        0
    }

    /// Byte offset of the available ring.
    #[must_use]
    pub const fn avail_offset(&self) -> usize {
        self.avail
    }

    /// Byte offset of the used ring.
    #[must_use]
    pub const fn used_offset(&self) -> usize {
        self.used
    }

    /// Total size of the queue in bytes.
    #[must_use]
    pub const fn total_bytes(&self) -> usize {
        self.total
    }

    /// How many 4 KiB pages the queue occupies.
    #[must_use]
    pub const fn pages(&self) -> usize {
        self.total.div_ceil(4096)
    }

    /// Byte offset of the available ring's `idx` field — the one word a driver
    /// updates on every submission.
    #[must_use]
    pub const fn avail_idx_offset(&self) -> usize {
        self.avail + 2
    }

    /// Byte offset of available-ring slot `slot`, which the driver fills with a
    /// descriptor index. Wraps: the ring is `size` entries and the index that
    /// selects a slot is free-running.
    #[must_use]
    pub const fn avail_ring_offset(&self, slot: u16) -> usize {
        self.avail + 4 + 2 * (slot % self.size) as usize
    }

    /// Byte offset of the used ring's `idx` field, which the *device* advances.
    #[must_use]
    pub const fn used_idx_offset(&self) -> usize {
        self.used + 2
    }

    /// Byte offset of used-ring element `slot`: an `id` (u32) then a `len` (u32).
    #[must_use]
    pub const fn used_ring_offset(&self, slot: u16) -> usize {
        self.used + 4 + 8 * (slot % self.size) as usize
    }
}

/// One `virtio-input` event: eight bytes, and the same three fields Linux calls
/// `type`, `code` and `value`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct InputEvent {
    /// Event class — see [`ev`].
    pub kind: u16,
    /// Which key, button or axis.
    pub code: u16,
    /// Key state (1 down, 0 up) or axis position.
    pub value: u32,
}

/// Bytes in one input event.
pub const INPUT_EVENT_BYTES: usize = 8;

impl InputEvent {
    /// Parse one event from the eight bytes a device wrote.
    ///
    /// Returns `None` for a short slice rather than reading past it: the length
    /// comes from the device's used ring, and a device that reports a short buffer
    /// must not be able to walk this driver off the end of its own memory.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < INPUT_EVENT_BYTES {
            return None;
        }
        Some(Self {
            kind: u16::from_le_bytes([bytes[0], bytes[1]]),
            code: u16::from_le_bytes([bytes[2], bytes[3]]),
            value: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        })
    }

    /// Whether this is a key or button going *down*.
    #[must_use]
    pub const fn is_key_press(&self) -> bool {
        self.kind == ev::KEY && self.value != 0
    }

    /// Every whole event in a buffer the device filled, in order.
    ///
    /// A device is allowed to pack several events into one buffer and a pointer
    /// always does: a single movement is `ABS_X`, `ABS_Y`, `SYN` — twenty-four
    /// bytes in one descriptor. A driver that parsed only the first would report
    /// horizontal motion and never vertical, and nothing in the log would say so,
    /// because every event it *did* report would be correct.
    ///
    /// A trailing partial event is dropped rather than guessed at, by the same rule
    /// [`parse`](InputEvent::parse) follows: the length came from the device.
    pub fn parse_all(bytes: &[u8]) -> impl Iterator<Item = Self> + '_ {
        bytes
            .chunks_exact(INPUT_EVENT_BYTES)
            .filter_map(Self::parse)
    }
}

/// Input event classes, as the Linux input layer numbers them — which is what
/// `virtio-input` passes through verbatim.
pub mod ev {
    /// A marker that ends a group of events belonging to one physical action.
    pub const SYN: u16 = 0x00;
    /// A key or button changed state.
    pub const KEY: u16 = 0x01;
    /// A relative axis moved (mouse).
    pub const REL: u16 = 0x02;
    /// An absolute axis moved (tablet, touchscreen).
    pub const ABS: u16 = 0x03;
}

/// Axis codes, for both [`ev::ABS`] and [`ev::REL`]: X is 0 and Y is 1 in both.
pub mod axis {
    /// Horizontal.
    pub const X: u16 = 0x00;
    /// Vertical.
    pub const Y: u16 = 0x01;
}

/// The button codes a pointer reports, which are [`ev::KEY`] events with codes
/// above the keyboard's range — the input layer makes no other distinction, and
/// neither does anything downstream of this driver.
pub mod btn {
    /// Left button, and the only one a `virtio-tablet` reports by default.
    pub const LEFT: u16 = 0x110;
    /// Right button.
    pub const RIGHT: u16 = 0x111;
    /// Middle button.
    pub const MIDDLE: u16 = 0x112;

    /// Whether a key code is a pointer button rather than a keyboard key.
    ///
    /// The boundary is `BTN_MISC` (0x100): everything below it is a key on a
    /// keyboard, everything from it up is a button on something you point with.
    /// A consumer that skipped this test would deliver a mouse click to whatever
    /// holds the keyboard focus, which is a window that may be nowhere near the
    /// pointer.
    #[must_use]
    pub const fn is_button(code: u16) -> bool {
        code >= 0x100
    }
}

/// The device-specific configuration space of a `virtio-input`, as offsets from
/// the device's base address.
///
/// This is how one input device is told from another. A keyboard and a tablet are
/// both `DeviceID` 18 and both answer every transport register identically; the
/// only thing that separates them is which event classes they claim here. A
/// manager that picked by slot order would work on the machine it was written on.
///
/// Reading it is a two-step conversation, not a read: write `SELECT` and `SUBSEL`,
/// then read `SIZE` — zero means "the device has nothing to say about that" — and
/// then `SIZE` bytes from `UNION`.
pub mod cfg {
    use super::reg;

    /// Which configuration item the device should expose.
    pub const SELECT: usize = reg::CONFIG;
    /// Which sub-item: the event class for [`EV_BITS`], the axis for [`ABS_INFO`].
    pub const SUBSEL: usize = reg::CONFIG + 1;
    /// How many bytes of [`UNION`] the device filled in. Zero is a complete answer:
    /// it means this device does not have the thing that was asked about.
    pub const SIZE: usize = reg::CONFIG + 2;
    /// Where the selected item's bytes begin — five reserved bytes after `SIZE`.
    pub const UNION: usize = reg::CONFIG + 8;
    /// How many bytes the union holds at most.
    pub const UNION_BYTES: usize = 128;

    /// Select nothing; the state a probe should leave behind it.
    pub const UNSET: u8 = 0x00;
    /// The device's name, as a string in the union.
    pub const ID_NAME: u8 = 0x01;
    /// Which codes of the event class in `SUBSEL` this device can report, as a
    /// bitmap. Size zero means it does not report that class at all.
    pub const EV_BITS: u8 = 0x11;
    /// The range of the absolute axis in `SUBSEL`, as an [`super::AbsInfo`].
    pub const ABS_INFO: u8 = 0x12;
}

/// The range of one absolute axis, as `VIRTIO_INPUT_CFG_ABS_INFO` reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AbsInfo {
    /// Smallest value the axis reports.
    pub min: u32,
    /// Largest value the axis reports.
    pub max: u32,
    /// Noise the device suggests ignoring; not used here, parsed so the layout is
    /// complete and a short answer is detected rather than guessed at.
    pub fuzz: u32,
    /// Dead zone around the centre.
    pub flat: u32,
    /// Resolution, in units per millimetre.
    pub res: u32,
}

/// Bytes in the `abs_info` structure.
pub const ABS_INFO_BYTES: usize = 20;

impl AbsInfo {
    /// Parse the five little-endian words the device wrote into the union.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < ABS_INFO_BYTES {
            return None;
        }
        let word = |i: usize| {
            u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]])
        };
        Some(Self {
            min: word(0),
            max: word(4),
            fuzz: word(8),
            flat: word(12),
            res: word(16),
        })
    }

    /// Where `value` sits on this axis, as a fraction of it scaled to
    /// `0..=`[`AXIS_SCALE`].
    ///
    /// The driver normalises rather than converting to pixels, because it is the
    /// only process that holds the axis range and it is *not* the process that
    /// knows the screen. Sending raw device units would make every consumer ask
    /// for the range; sending pixels would put the screen geometry in a keyboard
    /// driver. A fraction is the one form that needs neither.
    ///
    /// Saturating rather than wrapping: a device is entitled to report a value
    /// outside the range it advertised — QEMU's tablet does at the very edges —
    /// and a pointer that jumps to the opposite corner at the edge of the screen
    /// is a bug nobody would look for in arithmetic.
    #[must_use]
    pub fn normalise(&self, value: u32) -> u32 {
        // A degenerate range is what a device with no such axis reports, and
        // dividing by it would fault. The middle is the honest answer: there is no
        // position to report, and the corner would look like one.
        if self.max <= self.min {
            return AXIS_SCALE / 2;
        }
        let span = u64::from(self.max - self.min);
        let clamped = value.clamp(self.min, self.max);
        let offset = u64::from(clamped - self.min);
        // 64-bit throughout: `offset * AXIS_SCALE` overflows 32 bits for any range
        // wider than 65536, and a tablet advertising 0..32767 is one multiply away
        // from that.
        ((offset * u64::from(AXIS_SCALE)) / span) as u32
    }
}

/// The scale a normalised axis position uses: `0..=65535`, one axis position per
/// pixel of a screen far wider than any this will run on.
pub const AXIS_SCALE: u32 = 65535;

/// What kind of input device this is, decided by the event classes it claims.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    /// Keys, and no axes: a keyboard.
    Keyboard,
    /// Absolute axes: a tablet or a touchscreen. The position it reports *is* the
    /// position, so no pointer needs to be tracked between events.
    Tablet,
    /// Relative axes: a mouse. Every event is a delta from wherever the pointer
    /// was, so somebody downstream has to remember where that is.
    Mouse,
    /// Something this system has no use for.
    Other,
}

/// Classify a device from the three questions worth asking of `EV_BITS`.
///
/// Absolute before relative, and both before keys, because the interesting device
/// claims more than one: QEMU's `virtio-tablet` reports `EV_ABS` for the position
/// *and* `EV_KEY` for its button, so a test that asked about keys first would
/// call it a keyboard and route its clicks by focus.
#[must_use]
pub const fn classify(has_abs: bool, has_rel: bool, has_key: bool) -> InputKind {
    if has_abs {
        InputKind::Tablet
    } else if has_rel {
        InputKind::Mouse
    } else if has_key {
        InputKind::Keyboard
    } else {
        InputKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The alignment the legacy MMIO transport requires of the used ring.
    const LEGACY_ALIGN: usize = 4096;

    #[test]
    fn a_queue_of_eight_has_the_offsets_the_specification_gives() {
        let q = QueueLayout::new(8, LEGACY_ALIGN).unwrap();
        assert_eq!(q.desc_offset(), 0);
        // 8 descriptors * 16 bytes
        assert_eq!(q.avail_offset(), 128);
        // avail is 6 + 2*8 = 22 bytes, ending at 150, so the used ring is pushed
        // to the next 4096 boundary.
        assert_eq!(q.used_offset(), 4096);
        // used is 6 + 8*8 = 70 bytes
        assert_eq!(q.total_bytes(), 4096 + 70);
        assert_eq!(q.pages(), 2);
    }

    #[test]
    fn the_padding_before_the_used_ring_depends_on_the_size() {
        // The reason this is computed rather than a constant: two different queue
        // sizes put the used ring in the same place only by coincidence, and a
        // driver that hard-codes one is writing into the device's ring with the
        // other.
        for size in [1u16, 2, 4, 8, 16, 64, 256] {
            let q = QueueLayout::new(size, LEGACY_ALIGN).unwrap();
            let n = size as usize;
            let avail_end = n * DESC_BYTES + 6 + 2 * n;
            assert!(q.used_offset() >= avail_end, "used ring overlaps avail at size {size}");
            assert_eq!(q.used_offset() % LEGACY_ALIGN, 0, "used ring unaligned at size {size}");
            // And nothing is wasted beyond the alignment itself.
            assert!(q.used_offset() - avail_end < LEGACY_ALIGN);
        }
    }

    #[test]
    fn a_large_queue_pushes_the_used_ring_past_one_page() {
        // 256 descriptors are 4096 bytes on their own, so the available ring
        // starts exactly at a page boundary and the used ring lands on the *next*
        // one — the case where a fixed "used is at 4096" would be silently wrong.
        let q = QueueLayout::new(256, LEGACY_ALIGN).unwrap();
        assert_eq!(q.avail_offset(), 4096);
        assert_eq!(q.used_offset(), 8192);
    }

    #[test]
    fn impossible_queue_sizes_are_refused() {
        assert_eq!(QueueLayout::new(0, LEGACY_ALIGN), None, "zero");
        assert_eq!(QueueLayout::new(3, LEGACY_ALIGN), None, "not a power of two");
        assert_eq!(QueueLayout::new(1000, LEGACY_ALIGN), None, "not a power of two");
        // 32768 is the largest the specification allows; one more is not
        // representable as a u16 at all, so the boundary is the test.
        assert!(QueueLayout::new(32768, LEGACY_ALIGN).is_some());
        assert_eq!(QueueLayout::new(8, 0), None, "zero alignment");
        assert_eq!(QueueLayout::new(8, 3000), None, "alignment not a power of two");
    }

    #[test]
    fn ring_slots_wrap_at_the_queue_size_not_at_the_index() {
        // The index a driver keeps is free-running (it is a u16 that wraps at
        // 65536); the *slot* it selects is that index modulo the queue size.
        // Confusing the two writes past the ring, into the used ring's padding.
        let q = QueueLayout::new(8, LEGACY_ALIGN).unwrap();
        assert_eq!(q.avail_ring_offset(0), q.avail_offset() + 4);
        assert_eq!(q.avail_ring_offset(8), q.avail_ring_offset(0));
        assert_eq!(q.avail_ring_offset(9), q.avail_ring_offset(1));
        assert_eq!(q.used_ring_offset(8), q.used_ring_offset(0));
        // Every slot stays inside the region it belongs to.
        for slot in 0..64u16 {
            let a = q.avail_ring_offset(slot);
            assert!(a >= q.avail_offset() && a < q.used_offset());
            let u = q.used_ring_offset(slot);
            assert!(u >= q.used_offset() && u + 8 <= q.total_bytes());
        }
    }

    #[test]
    fn the_index_fields_are_where_the_rings_say_they_are() {
        let q = QueueLayout::new(16, LEGACY_ALIGN).unwrap();
        // flags first, then idx: two bytes in.
        assert_eq!(q.avail_idx_offset(), q.avail_offset() + 2);
        assert_eq!(q.used_idx_offset(), q.used_offset() + 2);
    }

    #[test]
    fn a_descriptor_is_sixteen_bytes_in_the_documented_order() {
        assert_eq!(core::mem::size_of::<Descriptor>(), DESC_BYTES);
        // The device reads this memory directly, so the field order is not an
        // implementation detail: addr, len, flags, next.
        let d = Descriptor {
            addr: 0x1234_5678_9abc_def0,
            len: 0x1122_3344,
            flags: DESC_F_WRITE,
            next: 7,
        };
        // SAFETY(test): reading a `repr(C)` POD struct as bytes on the host.
        let raw: [u8; DESC_BYTES] = unsafe { core::mem::transmute(d) };
        assert_eq!(&raw[0..8], &0x1234_5678_9abc_def0u64.to_le_bytes());
        assert_eq!(&raw[8..12], &0x1122_3344u32.to_le_bytes());
        assert_eq!(&raw[12..14], &2u16.to_le_bytes());
        assert_eq!(&raw[14..16], &7u16.to_le_bytes());
    }

    #[test]
    fn an_input_event_parses_the_way_the_device_writes_it() {
        // EV_KEY, code 30 ('a'), value 1 (pressed).
        let bytes = [0x01, 0x00, 0x1e, 0x00, 0x01, 0x00, 0x00, 0x00];
        let e = InputEvent::parse(&bytes).unwrap();
        assert_eq!(e.kind, ev::KEY);
        assert_eq!(e.code, 30);
        assert_eq!(e.value, 1);
        assert!(e.is_key_press());

        // The same key going up is not a press.
        let up = InputEvent::parse(&[0x01, 0x00, 0x1e, 0x00, 0x00, 0x00, 0x00, 0x00]).unwrap();
        assert!(!up.is_key_press());
        // Nor is a sync marker, whatever its value.
        let syn = InputEvent::parse(&[0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00]).unwrap();
        assert!(!syn.is_key_press());
    }

    #[test]
    fn a_short_event_buffer_is_refused_rather_than_read_past() {
        // The length comes from the device's used ring. A device reporting seven
        // bytes must not walk the driver off the end of its own buffer.
        assert_eq!(InputEvent::parse(&[0; 7]), None);
        assert_eq!(InputEvent::parse(&[]), None);
        // Extra bytes are fine: several events arrive in one buffer.
        assert!(InputEvent::parse(&[0; 64]).is_some());
    }

    #[test]
    fn the_magic_register_spells_virt() {
        assert_eq!(MAGIC_VALUE.to_le_bytes(), *b"virt");
    }

    #[test]
    fn one_buffer_can_hold_a_whole_pointer_movement() {
        // What a tablet actually sends for one movement, in one descriptor:
        // ABS_X = 100, ABS_Y = 200, SYN_REPORT. A driver reading only the first
        // event moves the pointer horizontally for ever.
        let mut buf = [0u8; 24];
        buf[0..8].copy_from_slice(&[0x03, 0x00, 0x00, 0x00, 100, 0, 0, 0]);
        buf[8..16].copy_from_slice(&[0x03, 0x00, 0x01, 0x00, 200, 0, 0, 0]);
        buf[16..24].copy_from_slice(&[0x00; 8]);

        let events: Vec<InputEvent> = InputEvent::parse_all(&buf).collect();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0], InputEvent { kind: ev::ABS, code: axis::X, value: 100 });
        assert_eq!(events[1], InputEvent { kind: ev::ABS, code: axis::Y, value: 200 });
        assert_eq!(events[2].kind, ev::SYN);
    }

    #[test]
    fn a_partial_trailing_event_is_dropped_not_guessed_at() {
        // Twelve bytes: one whole event and half of another. The length comes from
        // the device's used ring, and half an event is not an event.
        let mut buf = [0u8; 12];
        buf[0..8].copy_from_slice(&[0x01, 0x00, 0x1e, 0x00, 0x01, 0, 0, 0]);
        let events: Vec<InputEvent> = InputEvent::parse_all(&buf).collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code, 30);
        assert!(InputEvent::parse_all(&[]).next().is_none());
    }

    #[test]
    fn abs_info_parses_the_five_words_the_device_writes() {
        // QEMU's virtio-tablet: 0..32767 on both axes, no fuzz, no flat, no res.
        let mut bytes = [0u8; ABS_INFO_BYTES];
        bytes[4..8].copy_from_slice(&32767u32.to_le_bytes());
        let info = AbsInfo::parse(&bytes).unwrap();
        assert_eq!(info.min, 0);
        assert_eq!(info.max, 32767);
        assert_eq!(info.fuzz, 0);
        // Nineteen bytes is not an abs_info, and reading one out of it would take
        // the last word from whatever follows in the union.
        assert_eq!(AbsInfo::parse(&bytes[..19]), None);
    }

    #[test]
    fn an_axis_normalises_across_its_whole_range() {
        let info = AbsInfo { min: 0, max: 32767, ..AbsInfo::default() };
        assert_eq!(info.normalise(0), 0);
        assert_eq!(info.normalise(32767), AXIS_SCALE, "the far edge must reach the far edge");
        // The middle, within one part in sixty-five thousand.
        let middle = info.normalise(16383);
        assert!(middle.abs_diff(AXIS_SCALE / 2) <= 2, "middle was {middle}");
    }

    #[test]
    fn an_axis_that_does_not_start_at_zero_still_normalises() {
        // A touchscreen calibrated to a sub-range is the ordinary case on real
        // hardware, and treating `value` as if `min` were zero puts the pointer
        // permanently past the right edge.
        let info = AbsInfo { min: 1000, max: 5000, ..AbsInfo::default() };
        assert_eq!(info.normalise(1000), 0);
        assert_eq!(info.normalise(5000), AXIS_SCALE);
        assert_eq!(info.normalise(3000), AXIS_SCALE / 2);
    }

    #[test]
    fn an_axis_saturates_rather_than_wrapping_outside_its_range() {
        // A device may report past what it advertised. Wrapping here would send the
        // pointer to the opposite corner at the edge of the screen — a jump nobody
        // would look for in a division.
        let info = AbsInfo { min: 100, max: 200, ..AbsInfo::default() };
        assert_eq!(info.normalise(0), 0);
        assert_eq!(info.normalise(u32::MAX), AXIS_SCALE);
        // And a degenerate range — what a device with no such axis reports —
        // answers the middle rather than dividing by zero.
        let none = AbsInfo::default();
        assert_eq!(none.normalise(1234), AXIS_SCALE / 2);
    }

    #[test]
    fn a_wide_axis_does_not_overflow_the_scaling_multiply() {
        // `offset * AXIS_SCALE` leaves 32 bits for any range wider than 65536, and
        // a device advertising a million units is legal. In 32-bit arithmetic this
        // returns a small number for a large position.
        let info = AbsInfo { min: 0, max: 1_000_000, ..AbsInfo::default() };
        assert_eq!(info.normalise(1_000_000), AXIS_SCALE);
        assert_eq!(info.normalise(500_000), AXIS_SCALE / 2);
        let wide = AbsInfo { min: 0, max: u32::MAX, ..AbsInfo::default() };
        assert_eq!(wide.normalise(u32::MAX), AXIS_SCALE);
    }

    #[test]
    fn a_tablet_is_not_mistaken_for_a_keyboard() {
        // QEMU's virtio-tablet claims EV_ABS *and* EV_KEY, because its button is a
        // key event. Asking about keys first calls it a keyboard, and then its
        // clicks are routed by keyboard focus to a window that may be elsewhere.
        assert_eq!(classify(true, false, true), InputKind::Tablet);
        assert_eq!(classify(false, true, true), InputKind::Mouse);
        assert_eq!(classify(false, false, true), InputKind::Keyboard);
        assert_eq!(classify(false, false, false), InputKind::Other);
    }

    #[test]
    fn a_button_is_told_from_a_key_by_its_code() {
        assert!(btn::is_button(btn::LEFT));
        assert!(btn::is_button(btn::RIGHT));
        // Key code 30 is 'a', and the highest ordinary key code is still below the
        // button range.
        assert!(!btn::is_button(30));
        assert!(!btn::is_button(0xff));
    }

    #[test]
    fn the_configuration_space_is_where_the_specification_puts_it() {
        // These four offsets are the whole of how one input device is told from
        // another, and every one of them is silent when wrong: a `SIZE` read from
        // the wrong byte answers zero, which reads as "this device does not do
        // that" rather than as a mistake.
        assert_eq!(cfg::SELECT, 0x100);
        assert_eq!(cfg::SUBSEL, 0x101);
        assert_eq!(cfg::SIZE, 0x102);
        // Five reserved bytes after SIZE, so the union starts on the eighth.
        assert_eq!(cfg::UNION, 0x108);
    }
}
