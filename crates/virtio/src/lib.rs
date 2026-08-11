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
}
