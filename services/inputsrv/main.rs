//! `inputsrv` — a virtio-input driver, entirely in EL0.
//!
//! The kernel has no idea these devices exist. It never reads their registers,
//! never touches their rings, and never sees an event. What it provides is three
//! primitives: a capability that maps one MMIO page, a capability that turns an
//! interrupt line into a notification, and physically-contiguous memory a device
//! can address. Everything else — the queues, the handshakes, the descriptors, the
//! event decoding — is this program.
//!
//! ## Two devices, one driver
//! Something to type on and something to point with. They are the same device as
//! far as virtio is concerned — both `DeviceID` 18, both driven through the same
//! registers — so the difference is not in how they are read but in what their
//! events *mean*, and that is one `match` rather than a second program. The device
//! manager tells this one apart from that one and says which is which when it hands
//! them over; a driver cannot work it out for itself, because working it out means
//! reading a device it has not been given yet.
//!
//! ## What makes this the first real user of `CreateDma`
//! A virtqueue is memory *the device reads by physical address*. The DMA path
//! existed to be tested before this; here it is load-bearing twice over: the rings
//! have to be contiguous because the device is given one address for all three, and
//! non-cacheable because the device writes the used ring while this program reads
//! it, with no cache maintenance between them.
//!
//! Both devices' queues live in **one** allocation, carved into per-device regions.
//! Not thrift: `MapDma` places every buffer at the same address, so a second
//! allocation would land on top of the first and the second device's ring would be
//! the first device's ring. One allocation makes the arithmetic explicit instead of
//! making the collision invisible.
//!
//! ## The handshake, in the order the specification requires
//! Status ACKNOWLEDGE, then DRIVER; select queue 0 (`eventq`); read its maximum
//! size; write the size we chose, the alignment, and the page frame the rings live
//! in; fill the queue with buffers for the device to write; status DRIVER_OK. Then
//! sleep on the notifications until a device says something.
//!
//! Legacy transport (version 1) is what QEMU's `virt` gives with `-device
//! virtio-keyboard-device`, and it is what this drives: the modern one moves the
//! queue registers around and negotiates features, neither of which buys anything
//! for a device with no features.
//!
//! ## What leaves here
//! Three kinds of message, to whoever holds the receiving end — which is the
//! display server, because it is the only process that knows which window is where.
//!
//! ```text
//! tag = 1 Key      words[0] = key code, words[1] = 1 down / 0 up
//! tag = 4 Pointer  words[0] = x, words[1] = y, each 0..=65535 across the device
//! tag = 5 Button   words[0] = button code, words[1] = 1 down / 0 up
//! ```
//!
//! A pointer position is a **fraction of the device**, not pixels. This program
//! holds the axis ranges and does not hold the screen geometry; the compositor
//! holds the screen and has no business knowing what a tablet calls its far edge.
//! Sending raw device units would make the compositor ask for the range, and
//! sending pixels would put a screen size in a driver.
//!
//! Motion is published on `SYN` and not before. A movement arrives as `ABS_X`,
//! `ABS_Y`, `SYN` — three events, and the first two are half a position each. A
//! driver that forwarded them separately would move the pointer to a corner between
//! every pair of axes, which on a fast device is a cursor that shakes.
//!
//! ## This driver does not exit
//! It used to, after one key press, because it existed to prove a path. A key can
//! be pressed for as long as the machine runs, and a driver that has gone by then
//! is a decoded keystroke with nowhere to go — the same failure the display server
//! had, one process upstream.

#![no_std]
#![no_main]

use core::arch::{asm, naked_asm};
use core::panic::PanicInfo;

use staros_virtio::{axis, btn, cfg, ev, reg, status};
use staros_virtio::{AbsInfo, Descriptor, InputEvent, QueueLayout};
use staros_virtio::{ABS_INFO_BYTES, DESC_F_WRITE, DEVICE_ID_INPUT, MAGIC_VALUE};

/// Where the DMA buffer holding every queue appears in our address space.
const USER_DMA_VA: u64 = 0x7_0000_0000;

// Syscall numbers — must match `staros_abi::syscall::Syscall`.
const SYS_SEND: usize = 1;
const SYS_RECV: usize = 2;
const SYS_MAP_MEMORY: usize = 3;
const SYS_EXIT: usize = 4;
const SYS_IRQ_REGISTER: usize = 7;
const SYS_IRQ_ACK: usize = 9;
const SYS_DEBUG_WRITE: usize = 19;
const SYS_CREATE_DMA: usize = 16;
const SYS_MAP_DMA: usize = 17;
const SYS_WAIT_ANY: usize = 24;
const SYS_DMA_PHYS: usize = 27;

/// The endpoint the device manager delegates our devices and interrupts on.
const EP_MANAGER: u64 = 1;
/// Where decoded events go. Send only: a driver publishes, it does not consume.
const EP_EVENTS: u64 = 2;

/// The kind tags the device manager uses when it hands a device over, and the
/// terminator that says no more are coming.
const KIND_END: u64 = 0;
const KIND_KEYBOARD: u64 = 1;
const KIND_TABLET: u64 = 2;
const KIND_MOUSE: u64 = 3;

/// The message tags this driver publishes. `TAG_KEY` keeps the Linux number for
/// `EV_KEY` because that is what it is; the other two are this system's, because a
/// normalised position and a raw axis event are not the same message.
const TAG_KEY: u64 = 1;
const TAG_POINTER: u64 = 4;
const TAG_BUTTON: u64 = 5;

/// The most devices this driver will accept. Two: a keyboard and a pointer.
const MAX_DEVICES: usize = 2;

/// Queue size. Eight buffers is more than a keyboard produces between two of our
/// wake-ups, and small enough that a queue is two pages.
const QUEUE_SIZE: u16 = 8;
/// Bytes per event buffer. One event is eight bytes; the device packs several into
/// one buffer whenever a movement happens, so give it room for four.
const EVENT_BUF_BYTES: usize = 32;
/// The legacy transport's required alignment for the used ring.
const QUEUE_ALIGN: usize = 4096;
/// Pages per device: the rings need two at this size, and the eight event buffers
/// fit in the third.
const PAGES_PER_DEVICE: u64 = 3;

/// A message, laid out exactly as `staros_ipc::Message` (four words, then the cap).
#[repr(C)]
struct Message {
    tag: u64,
    words: [u64; 4],
    cap: u32,
}

impl Message {
    const fn new() -> Self {
        Self {
            tag: 0,
            words: [0; 4],
            cap: 0,
        }
    }
}

/// One device this driver is driving.
struct Device {
    /// Its registers, at the offset within the page the kernel told us.
    mmio: u64,
    /// The interrupt capability, re-armed after every batch.
    irq_cap: u32,
    /// The notification `IrqRegister` gave us for that line.
    notif: u32,
    /// Where its queue lives, in our space, and the layout of it.
    queue: QueueLayout,
    rings: u64,
    buffers: u64,
    /// How far into the used ring we have read.
    last_used: u16,
    /// What the manager said this is.
    kind: u64,
    /// The axis ranges, read from the configuration space. A keyboard's are the
    /// degenerate ones, which normalise to the middle and are never sent.
    abs_x: AbsInfo,
    abs_y: AbsInfo,
    /// The position accumulated since the last `SYN`, and whether either axis moved
    /// in it. Both axes are remembered because a device sends only what changed: a
    /// purely horizontal movement is one `ABS_X` and a `SYN`, and a driver that
    /// treated the missing axis as zero would drag the pointer along the top edge.
    x: u32,
    y: u32,
    moved: bool,
}

impl Device {
    const fn empty() -> Self {
        Self {
            mmio: 0,
            irq_cap: 0,
            notif: 0,
            // A layout that cannot be built is not a device; this is a placeholder
            // for an array slot that is never read before `setup` overwrites it.
            queue: match QueueLayout::new(QUEUE_SIZE, QUEUE_ALIGN) {
                Some(q) => q,
                None => panic!("the queue size is a constant and it is valid"),
            },
            rings: 0,
            buffers: 0,
            last_used: 0,
            kind: KIND_END,
            abs_x: AbsInfo { min: 0, max: 0, fuzz: 0, flat: 0, res: 0 },
            abs_y: AbsInfo { min: 0, max: 0, fuzz: 0, flat: 0, res: 0 },
            x: 0,
            y: 0,
            moved: false,
        }
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
    // The queues for every device, in one contiguous non-cacheable run. Allocated
    // before we know how many devices there are, because the allocation is what
    // must not be repeated: a second `CreateDma` would map on top of this one.
    // SAFETY: plain syscalls; the kernel allocates and maps the run.
    let dma = unsafe { syscall1(SYS_CREATE_DMA, PAGES_PER_DEVICE * MAX_DEVICES as u64) };
    if dma < 0 {
        puts("[inputsrv] no DMA buffer for the queues\n");
        exit();
    }
    // SAFETY: as above; returns the VA the run is mapped at.
    let va = unsafe { syscall1(SYS_MAP_DMA, dma as u64) };
    if va < 0 || va as u64 != USER_DMA_VA {
        puts("[inputsrv] the DMA buffer did not map where expected\n");
        exit();
    }
    // The address the *devices* must be given. Not the one above: a device has no
    // page table, and the VA `MapDma` returned means nothing to it. Asking for the
    // physical address is what `DmaPhys` is for — a syscall this driver's existence
    // is the reason for, since every earlier user of the DMA path only ever read
    // and wrote the buffer itself.
    // SAFETY: plain syscall; the kernel resolves the capability we hold.
    let dma_phys = unsafe { syscall1(SYS_DMA_PHYS, dma as u64) };
    if dma_phys <= 0 {
        puts("[inputsrv] the kernel would not say where the DMA buffer lives\n");
        exit();
    }
    let dma_phys = dma_phys as u64;

    let mut devices = [Device::empty(), Device::empty()];
    let mut count = 0;
    while count < MAX_DEVICES {
        // Two messages per device — what it is with the device capability, then the
        // same kind with its interrupt — and a terminator when there are no more.
        let Some((kind, dev_cap)) = recv_cap() else {
            puts("[inputsrv] the device manager stopped talking mid-handover\n");
            exit();
        };
        if kind == KIND_END {
            break;
        }
        let Some((_, irq_cap)) = recv_cap() else {
            puts("[inputsrv] a device arrived without its interrupt\n");
            exit();
        };
        if setup(&mut devices[count], kind, dev_cap, irq_cap, dma_phys, count) {
            count += 1;
        }
    }
    if count == 0 {
        puts("[inputsrv] no input devices on this machine; nothing to drive\n");
        exit();
    }

    put_line(
        "[inputsrv] virtio-input driver up in EL0: ",
        count as u64,
        " device(s), queues armed, waiting\n",
    );

    // Live for as long as the machine does. See the module comment: a driver that
    // stops after proving itself is a driver that is absent when somebody types.
    loop {
        let handles = [devices[0].notif, devices[1].notif];
        // SAFETY: `WaitAny` reads `count` handles from this array and parks with no
        // deadline; the array outlives the call.
        let rc = unsafe {
            syscall3(SYS_WAIT_ANY, handles.as_ptr() as u64, count as u64, 0)
        };
        if rc < 0 {
            puts("[inputsrv] the wait failed; the driver is going down\n");
            exit();
        }
        // Every device is drained, not only the one the wait named. `WaitAny`
        // consumes one signal, and two devices interrupting at once produce two
        // signals and one wake-up: draining only the named one leaves the other's
        // events in its ring until it happens to interrupt again, which for a
        // keyboard is the next keystroke — so the previous one arrives late and out
        // of order.
        for device in &mut devices[..count] {
            drain(device);
        }
    }
}

/// Bring one device up: map it, check it, build its queue, arm its interrupt.
/// `false` if anything about it is not what it claimed to be.
fn setup(
    device: &mut Device,
    kind: u64,
    dev_cap: u32,
    irq_cap: u32,
    dma_phys: u64,
    slot: usize,
) -> bool {
    // SAFETY: `MapMemory` maps the MMIO page the capability names into our space.
    // One placement per device object, so the second device does not land on the
    // first — which it did, silently, until the kernel started remembering.
    let mmio = unsafe { syscall1(SYS_MAP_MEMORY, u64::from(dev_cap)) };
    if mmio < 0 {
        puts("[inputsrv] a device page would not map\n");
        return false;
    }
    let mmio = mmio as u64;

    // SAFETY: the page is Device memory mapped read/write for us.
    // The version register is read and not checked: this driver speaks the legacy
    // transport, and a device announcing the modern one would fail at the queue
    // registers, which have moved. Reading it keeps the register map honest — the
    // offsets are asserted by the crate's tests — without pretending to a check
    // that is not made.
    let (magic, _version, device_id) = unsafe {
        (
            read32(mmio, reg::MAGIC),
            read32(mmio, reg::VERSION),
            read32(mmio, reg::DEVICE_ID),
        )
    };
    if magic != MAGIC_VALUE || device_id != DEVICE_ID_INPUT {
        puts("[inputsrv] that is not a virtio-input device\n");
        return false;
    }

    let Some(queue) = QueueLayout::new(QUEUE_SIZE, QUEUE_ALIGN) else {
        puts("[inputsrv] impossible queue size\n");
        return false;
    };

    // This device's slice of the one DMA run. Page-aligned because `QUEUE_PFN` is
    // written as a page frame number and a queue that started mid-page would be
    // handed to the device rounded down — onto the previous device's used ring.
    let region = slot as u64 * PAGES_PER_DEVICE * 4096;
    let rings = USER_DMA_VA + region;
    let phys = dma_phys + region;

    // SAFETY: writing the transport's registers in the order the specification
    // requires; the page is ours and Device-mapped.
    unsafe {
        write32(mmio, reg::STATUS, 0); // reset
        write32(mmio, reg::STATUS, status::ACKNOWLEDGE);
        write32(mmio, reg::STATUS, status::ACKNOWLEDGE | status::DRIVER);
        // No features: an input device needs none of them, and accepting none is
        // both legal and the smallest thing that can go wrong.
        write32(mmio, reg::DRIVER_FEATURES_SEL, 0);
        write32(mmio, reg::DRIVER_FEATURES, 0);
        write32(mmio, reg::GUEST_PAGE_SIZE, 4096);
        write32(mmio, reg::QUEUE_SEL, 0); // eventq
    }
    // SAFETY: as above.
    let max = unsafe { read32(mmio, reg::QUEUE_NUM_MAX) };
    if max == 0 {
        puts("[inputsrv] the event queue is unusable\n");
        return false;
    }
    // SAFETY: as above.
    unsafe {
        write32(mmio, reg::QUEUE_NUM, u32::from(QUEUE_SIZE.min(max as u16)));
        write32(mmio, reg::QUEUE_ALIGN, QUEUE_ALIGN as u32);
        write32(mmio, reg::QUEUE_PFN, (phys / 4096) as u32);
    }

    // Fill the queue: every descriptor points at one event buffer and is marked
    // device-writable. The buffers live past the rings, in the same region.
    // Two addresses for the same memory, and keeping them apart is the whole of
    // what a driver does differently from an ordinary program: the descriptors
    // carry the *physical* one, because the device dereferences it, and this
    // program reads the events through the *virtual* one.
    let buffers_off = (queue.total_bytes() as u64 + 63) & !63;
    let buffers_phys = phys + buffers_off;
    let buffers = rings + buffers_off;
    for i in 0..QUEUE_SIZE {
        let desc = Descriptor {
            addr: buffers_phys + u64::from(i) * EVENT_BUF_BYTES as u64,
            len: EVENT_BUF_BYTES as u32,
            flags: DESC_F_WRITE,
            next: 0,
        };
        // SAFETY: inside this device's region of the DMA run, which is
        // `PAGES_PER_DEVICE` pages and mapped for us.
        unsafe {
            let slot = (rings + (i as usize * 16) as u64) as *mut Descriptor;
            slot.write_volatile(desc);
            // And publish it in the available ring.
            let ring = (rings + queue.avail_ring_offset(i) as u64) as *mut u16;
            ring.write_volatile(i);
        }
    }
    // SAFETY: the available ring's index, made visible to the device.
    unsafe {
        let idx = (rings + queue.avail_idx_offset() as u64) as *mut u16;
        idx.write_volatile(QUEUE_SIZE);
        write32(mmio, reg::STATUS, status::ACKNOWLEDGE | status::DRIVER | status::DRIVER_OK);
        write32(mmio, reg::QUEUE_NOTIFY, 0);
    }

    // Ask the kernel to turn our interrupt capability into a notification.
    // SAFETY: `IrqRegister` enables the line and returns a notification handle.
    let notif = unsafe { syscall1(SYS_IRQ_REGISTER, u64::from(irq_cap)) };
    if notif < 0 {
        puts("[inputsrv] an interrupt would not register\n");
        return false;
    }

    device.mmio = mmio;
    device.irq_cap = irq_cap;
    device.notif = notif as u32;
    device.queue = queue;
    device.rings = rings;
    device.buffers = buffers;
    device.last_used = 0;
    device.kind = kind;
    // A pointer's axis ranges, read *after* the handshake because the configuration
    // space is only guaranteed to answer once the device knows a driver is there.
    if kind == KIND_TABLET {
        device.abs_x = abs_info(mmio, axis::X);
        device.abs_y = abs_info(mmio, axis::Y);
        put_line(
            "[inputsrv] a tablet: absolute axes 0..",
            u64::from(device.abs_x.max),
            " wide, reported as a fraction so the compositor keeps the screen size\n",
        );
    } else if kind == KIND_KEYBOARD {
        puts("[inputsrv] a keyboard\n");
    } else if kind == KIND_MOUSE {
        puts("[inputsrv] a mouse: relative axes, which nothing consumes yet\n");
    }
    true
}

/// Read one absolute axis's range out of the configuration space.
///
/// A size of zero means the device has no such axis, and the degenerate range this
/// returns for it normalises to the middle rather than dividing by nothing. That is
/// the honest answer: a device with no Y axis has no Y position, and a corner would
/// look like one.
fn abs_info(mmio: u64, which: u16) -> AbsInfo {
    let mut bytes = [0u8; ABS_INFO_BYTES];
    // SAFETY: `mmio` is our mapped device page; these offsets are inside it, and
    // the two writes are the documented way to select a configuration item.
    let size = unsafe {
        ((mmio + cfg::SELECT as u64) as *mut u8).write_volatile(cfg::ABS_INFO);
        ((mmio + cfg::SUBSEL as u64) as *mut u8).write_volatile(which as u8);
        ((mmio + cfg::SIZE as u64) as *const u8).read_volatile() as usize
    };
    if size < ABS_INFO_BYTES {
        return AbsInfo { min: 0, max: 0, fuzz: 0, flat: 0, res: 0 };
    }
    for (i, byte) in bytes.iter_mut().enumerate() {
        // SAFETY: `i < ABS_INFO_BYTES <= size`, and the union is at least that long.
        *byte = unsafe { ((mmio + cfg::UNION as u64 + i as u64) as *const u8).read_volatile() };
    }
    AbsInfo::parse(&bytes).unwrap_or(AbsInfo { min: 0, max: 0, fuzz: 0, flat: 0, res: 0 })
}

/// Read everything one device has produced since last time, publish it, and give
/// the buffers back.
fn drain(device: &mut Device) {
    // SAFETY: acknowledging at the transport, then reading the used ring the device
    // advanced. Acknowledging before reading is deliberate: an event that arrives
    // between the two is still in the ring and is read on this pass, whereas
    // acknowledging afterwards can lose the interrupt for it entirely.
    unsafe {
        let isr = read32(device.mmio, reg::INTERRUPT_STATUS);
        write32(device.mmio, reg::INTERRUPT_ACK, isr);
    }
    // SAFETY: inside the mapped DMA run.
    let used_idx = unsafe {
        ((device.rings + device.queue.used_idx_offset() as u64) as *const u16).read_volatile()
    };
    // No early return when the ring has not moved, and this is the whole of why two
    // devices are harder than one.
    //
    // Every wake-up drains *both* devices, so a device whose ring is empty is the
    // ordinary case — the other one interrupted. `IrqAck` at the bottom is what
    // re-enables the line, and returning here skips it: the device is left masked,
    // its next event raises an interrupt nobody hears, and it goes silent for ever
    // while the other keeps working. The symptom was a keyboard that stopped after
    // the pointer moved, or a pointer that stopped after a keystroke — whichever
    // lost the race — and it looked exactly like a flaky test.
    while device.last_used != used_idx {
        // SAFETY: as above; each used element is an (id, len) pair.
        let (id, len) = unsafe {
            let e = (device.rings + device.queue.used_ring_offset(device.last_used) as u64)
                as *const u32;
            (e.read_volatile(), e.add(1).read_volatile())
        };
        handle_buffer(device, id, len);
        // Hand the buffer straight back: the device may refill it.
        // SAFETY: republishing the descriptor in the available ring.
        unsafe {
            let ring =
                (device.rings + device.queue.avail_ring_offset(device.last_used) as u64) as *mut u16;
            ring.write_volatile(id as u16);
            let idx = (device.rings + device.queue.avail_idx_offset() as u64) as *mut u16;
            idx.write_volatile(idx.read_volatile().wrapping_add(1));
        }
        device.last_used = device.last_used.wrapping_add(1);
    }
    // SAFETY: tell the device there are buffers again, then re-enable the line.
    unsafe {
        write32(device.mmio, reg::QUEUE_NOTIFY, 0);
        syscall1(SYS_IRQ_ACK, u64::from(device.irq_cap));
    }
}

/// Decode every event in one filled buffer.
///
/// Every event, not the first. A device packs a whole movement into one descriptor
/// — `ABS_X`, `ABS_Y`, `SYN` — and a driver that read one event per buffer reports
/// horizontal motion and never vertical, with nothing in the log to say so, because
/// everything it does report is correct.
fn handle_buffer(device: &mut Device, id: u32, len: u32) {
    if id >= u32::from(QUEUE_SIZE) {
        return;
    }
    let base = device.buffers + u64::from(id) * EVENT_BUF_BYTES as u64;
    let len = (len as usize).min(EVENT_BUF_BYTES);
    // SAFETY: `base` is one of our own event buffers, inside the DMA run, and `len`
    // is clamped to its size.
    let bytes = unsafe { core::slice::from_raw_parts(base as *const u8, len) };
    // Collected first, then handled: the borrow of the buffer ends before anything
    // is published, and publishing may block.
    let mut events = [InputEvent::default(); EVENT_BUF_BYTES / 8];
    let mut n = 0;
    for event in InputEvent::parse_all(bytes) {
        events[n] = event;
        n += 1;
    }
    for event in &events[..n] {
        handle_event(device, *event);
    }
}

/// Turn one decoded event into a message, or into a piece of one.
fn handle_event(device: &mut Device, event: InputEvent) {
    match event.kind {
        ev::KEY if btn::is_button(event.code) => {
            publish(TAG_BUTTON, u64::from(event.code), u64::from(event.value));
        }
        ev::KEY => {
            if event.is_key_press() {
                put_line(
                    "[inputsrv] key press from the device: code ",
                    u64::from(event.code),
                    " - decoded in EL0, the kernel never saw the event\n",
                );
            }
            publish(TAG_KEY, u64::from(event.code), u64::from(event.value));
        }
        ev::ABS if event.code == axis::X => {
            device.x = device.abs_x.normalise(event.value);
            device.moved = true;
        }
        ev::ABS if event.code == axis::Y => {
            device.y = device.abs_y.normalise(event.value);
            device.moved = true;
        }
        // The end of one physical action, and the only moment a position is whole.
        ev::SYN => {
            if device.moved {
                device.moved = false;
                publish(TAG_POINTER, u64::from(device.x), u64::from(device.y));
            }
        }
        // Relative axes are decoded and dropped, on purpose. Turning a delta into a
        // position needs somewhere to keep the pointer, and the process that keeps
        // it is the compositor — this driver would be inventing a second one that
        // disagreed with it. The day a mouse is attached, the delta goes in a
        // message of its own rather than in this one.
        _ => {}
    }
}

/// Publish one decoded event to whoever holds the receiving end.
///
/// The send is deliberately allowed to block. An endpoint's ring is small, and a
/// consumer that has stopped draining is a consumer that will lose events either
/// way — but a driver that *drops* them silently produces a keyboard which
/// occasionally misses a keystroke, which is the hardest class of bug there is to
/// believe. Blocking makes the back-pressure visible instead: the driver stops
/// acknowledging interrupts, the device's queue fills, and the failure has a shape.
fn publish(tag: u64, first: u64, second: u64) {
    let mut msg = Message::new();
    msg.tag = tag;
    msg.words[0] = first;
    msg.words[1] = second;
    // SAFETY: `Send` reads one `Message` through this pointer; the endpoint handle
    // is the capability the kernel installed for exactly this.
    unsafe {
        let _ = syscall2(SYS_SEND, EP_EVENTS, core::ptr::addr_of!(msg) as u64);
    }
}

/// Receive one capability from the device manager, with the tag saying what it is.
fn recv_cap() -> Option<(u64, u32)> {
    let mut msg = Message::new();
    // SAFETY: `Recv` writes one `Message` through this pointer.
    let rc = unsafe { syscall2(SYS_RECV, EP_MANAGER, core::ptr::addr_of_mut!(msg) as u64) };
    if rc < 0 {
        return None;
    }
    // The terminator carries no capability, and that is the one message where a
    // zero handle is not a failure.
    if msg.tag == KIND_END {
        return Some((KIND_END, 0));
    }
    if msg.cap == 0 {
        return None;
    }
    Some((msg.tag, msg.cap))
}

/// Read a 32-bit device register.
///
/// # Safety
/// `base` must be a mapped virtio-mmio page and `offset` inside it.
unsafe fn read32(base: u64, offset: usize) -> u32 {
    // SAFETY: forwarded to the caller.
    unsafe { ((base + offset as u64) as *const u32).read_volatile() }
}

/// Write a 32-bit device register.
///
/// # Safety
/// As [`read32`], and the value must be one the register accepts.
unsafe fn write32(base: u64, offset: usize, value: u32) {
    // SAFETY: forwarded to the caller.
    unsafe { ((base + offset as u64) as *mut u32).write_volatile(value) }
}

/// Write a string to the debug console in one syscall.
fn puts(s: &str) {
    // SAFETY: `DebugWrite` reads `len` bytes from `ptr` after walking our tables.
    unsafe {
        let _ = syscall2(SYS_DEBUG_WRITE, s.as_ptr() as u64, s.len() as u64);
    }
}

/// Print `prefix`, a number, and `suffix` as **one** line, in one `DebugWrite`.
///
/// Three calls would be three chances for another task to write between them, and
/// the result reads `key press from the device: code [ipc-storm] receiver drained…`.
/// That is not hypothetical — it is what this function replaced, and it had passed
/// for several runs before it did. One call is one line as far as the kernel's
/// console lock is concerned; a line built from three is a line that is *usually*
/// whole, which is the worst kind of test.
fn put_line(prefix: &str, mut value: u64, suffix: &str) {
    let mut line = [0u8; 160];
    let mut n = 0;
    let mut push = |bytes: &[u8], line: &mut [u8; 160], n: &mut usize| {
        for &b in bytes {
            if *n < line.len() {
                line[*n] = b;
                *n += 1;
            }
        }
    };
    push(prefix.as_bytes(), &mut line, &mut n);
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    push(&digits[i..], &mut line, &mut n);
    push(suffix.as_bytes(), &mut line, &mut n);
    // SAFETY: as `puts`; `n` bytes of a buffer we own.
    unsafe {
        let _ = syscall2(SYS_DEBUG_WRITE, line.as_ptr() as u64, n as u64);
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
/// The number and argument must name a syscall this process may make.
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
