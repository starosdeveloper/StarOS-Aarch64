//! `inputsrv` — a virtio-input driver, entirely in EL0.
//!
//! The kernel has no idea this device exists. It never reads its registers, never
//! touches its rings, and never sees an event. What it provides is three
//! primitives: a capability that maps one MMIO page, a capability that turns an
//! interrupt line into a notification, and physically-contiguous memory a device
//! can address. Everything else — the queue, the handshake, the descriptors, the
//! event decoding — is this program.
//!
//! ## What makes this the first real user of `CreateDma`
//! A virtqueue is memory *the device reads by physical address*. Until now the DMA
//! path existed to be tested; here it is load-bearing twice over: the rings have
//! to be contiguous because the device is given one address for all three, and
//! non-cacheable because the device writes the used ring while this program reads
//! it, with no cache maintenance between them.
//!
//! ## The handshake, in the order the specification requires
//! Status ACKNOWLEDGE, then DRIVER; select queue 0 (`eventq`); read its maximum
//! size; write the size we chose, the alignment, and the page frame the rings live
//! in; fill the queue with buffers for the device to write; status DRIVER_OK. Then
//! sleep on the notification until the device says something.
//!
//! Legacy transport (version 1) is what QEMU's `virt` gives with `-device
//! virtio-keyboard-device`, and it is what this drives: the modern one moves the
//! queue registers around and negotiates features, neither of which buys anything
//! for a device with no features.

#![no_std]
#![no_main]

use core::arch::{asm, naked_asm};
use core::panic::PanicInfo;

use staros_virtio::{ev, reg, status, Descriptor, InputEvent, QueueLayout};
use staros_virtio::{DESC_F_WRITE, DEVICE_ID_INPUT, MAGIC_VALUE};

/// Where a mapped DMA buffer appears in our address space.
const USER_DMA_VA: u64 = 0x7_0000_0000;

// Syscall numbers — must match `staros_abi::syscall::Syscall`.
const SYS_SEND: usize = 1;
const SYS_RECV: usize = 2;
const SYS_MAP_MEMORY: usize = 3;
const SYS_EXIT: usize = 4;
const SYS_IRQ_REGISTER: usize = 7;
const SYS_WAIT: usize = 8;
const SYS_IRQ_ACK: usize = 9;
const SYS_DEBUG_WRITE: usize = 19;
const SYS_CREATE_DMA: usize = 16;
const SYS_MAP_DMA: usize = 17;
const SYS_DMA_PHYS: usize = 27;

/// The endpoint the device manager delegates our device and interrupt on.
const EP_MANAGER: u64 = 1;
/// Where decoded events go. Send only: a driver publishes, it does not consume.
const EP_EVENTS: u64 = 2;

/// The event kinds this driver publishes, as the message tag. They are the Linux
/// numbers virtio-input passes through unchanged, so a consumer that already knows
/// `EV_KEY` needs no translation table.
const TAG_KEY: u64 = 1;
const TAG_REL: u64 = 2;
const TAG_ABS: u64 = 3;

/// Queue size. Eight buffers is more than a keyboard produces between two of our
/// wake-ups, and small enough that the whole queue is two pages.
const QUEUE_SIZE: u16 = 8;
/// Bytes per event buffer. One event is eight bytes; the device may pack several
/// into one buffer, so give it room for four.
const EVENT_BUF_BYTES: usize = 32;
/// The legacy transport's required alignment for the used ring.
const QUEUE_ALIGN: usize = 4096;
/// Pages for rings plus buffers. The rings need two pages at this size; the eight
/// event buffers fit in the third.
const DMA_PAGES: u64 = 3;
/// How many key presses to report before this driver has proved its point and
/// exits, so the demo terminates on its own.
const PRESSES_WANTED: u32 = 1;

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

/// Entry point: linked at `USER_BASE`, entered by the kernel with a fresh stack.
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.start"]
extern "C" fn _start() -> ! {
    naked_asm!("bl {main}", "b .", main = sym main)
}

extern "C" fn main() -> ! {
    // Two capabilities arrive over IPC: the device page, then its interrupt line.
    // We hold no authority of our own — a driver is only ever as capable as what
    // it was handed.
    let Some(dev_cap) = recv_cap() else {
        puts("[inputsrv] no device capability arrived\n");
        exit();
    };
    let Some(irq_cap) = recv_cap() else {
        puts("[inputsrv] no interrupt capability arrived\n");
        exit();
    };

    // SAFETY: `MapMemory` maps the MMIO page the capability names into our space.
    let mmio = unsafe { syscall1(SYS_MAP_MEMORY, u64::from(dev_cap)) };
    if mmio < 0 {
        puts("[inputsrv] the device page would not map\n");
        exit();
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
        exit();
    }

    let Some(queue) = QueueLayout::new(QUEUE_SIZE, QUEUE_ALIGN) else {
        puts("[inputsrv] impossible queue size\n");
        exit();
    };

    // The rings and the event buffers, in one physically-contiguous,
    // non-cacheable allocation — the two properties a device sharing memory with
    // us needs and ordinary memory does not have.
    // SAFETY: plain syscalls; the kernel allocates and maps the run.
    let dma = unsafe { syscall1(SYS_CREATE_DMA, DMA_PAGES) };
    if dma < 0 {
        puts("[inputsrv] no DMA buffer for the queue\n");
        exit();
    }
    // SAFETY: as above; returns the VA the run is mapped at.
    let va = unsafe { syscall1(SYS_MAP_DMA, dma as u64) };
    if va < 0 || va as u64 != USER_DMA_VA {
        puts("[inputsrv] the DMA buffer did not map where expected\n");
        exit();
    }
    let rings = USER_DMA_VA;
    // The address the *device* must be given. Not the one above: a device has no
    // page table, and the VA `MapDma` returned means nothing to it. Asking for the
    // physical address is what `DmaPhys` is for — a syscall this driver's existence
    // is the reason for, since every earlier user of the DMA path only ever read
    // and wrote the buffer itself.
    // SAFETY: plain syscall; the kernel resolves the capability we hold.
    let phys = unsafe { syscall1(SYS_DMA_PHYS, dma as u64) };
    if phys <= 0 {
        puts("[inputsrv] the kernel would not say where the DMA buffer lives\n");
        exit();
    }
    let phys = phys as u64;

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
        exit();
    }
    // SAFETY: as above.
    unsafe {
        write32(mmio, reg::QUEUE_NUM, u32::from(QUEUE_SIZE.min(max as u16)));
        write32(mmio, reg::QUEUE_ALIGN, QUEUE_ALIGN as u32);
        write32(mmio, reg::QUEUE_PFN, (phys / 4096) as u32);
    }

    // Fill the queue: every descriptor points at one event buffer and is marked
    // device-writable. The buffers live past the rings, in the same allocation.
    // Two addresses for the same memory, and keeping them apart is the whole of
    // what a driver does differently from an ordinary program: the descriptors
    // carry the *physical* one, because the device dereferences it, and this
    // program reads the events through the *virtual* one.
    let buffers_off = (queue.total_bytes() as u64 + 63) & !63;
    let buffers_phys = phys + buffers_off;
    let buffers_va = rings + buffers_off;
    for i in 0..QUEUE_SIZE {
        let desc = Descriptor {
            addr: buffers_phys + u64::from(i) * EVENT_BUF_BYTES as u64,
            len: EVENT_BUF_BYTES as u32,
            flags: DESC_F_WRITE,
            next: 0,
        };
        // SAFETY: inside the DMA run, which is `DMA_PAGES` pages and mapped for us.
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
        puts("[inputsrv] the interrupt would not register\n");
        exit();
    }

    puts("[inputsrv] virtio-input driver up in EL0: queue armed, waiting for the device\n");

    let mut last_used = 0u16;
    let mut presses = 0;
    while presses < PRESSES_WANTED {
        // SAFETY: blocks until the kernel forwards this device's interrupt.
        let _ = unsafe { syscall1(SYS_WAIT, notif as u64) };
        // SAFETY: acknowledging at the transport, then reading the used ring the
        // device just advanced.
        unsafe {
            let isr = read32(mmio, reg::INTERRUPT_STATUS);
            write32(mmio, reg::INTERRUPT_ACK, isr);
        }
        // SAFETY: inside the mapped DMA run.
        let used_idx = unsafe { ((rings + queue.used_idx_offset() as u64) as *const u16).read_volatile() };
        while last_used != used_idx {
            // SAFETY: as above; each used element is an (id, len) pair.
            let (id, len) = unsafe {
                let e = (rings + queue.used_ring_offset(last_used) as u64) as *const u32;
                (e.read_volatile(), e.add(1).read_volatile())
            };
            if let Some(event) = read_event(buffers_va, id, len) {
                if event.is_key_press() {
                    puts("[inputsrv] key press from the device: code ");
                    put_dec(u64::from(event.code));
                    puts(" - decoded in EL0, the kernel never saw the event\n");
                    publish(TAG_KEY, u64::from(event.code), u64::from(event.value));
                    presses += 1;
                } else if event.kind == ev::REL || event.kind == ev::ABS {
                    puts("[inputsrv] pointer motion from the device\n");
                    let tag = if event.kind == ev::REL { TAG_REL } else { TAG_ABS };
                    publish(tag, u64::from(event.code), u64::from(event.value));
                }
            }
            // Hand the buffer straight back: the device may refill it.
            // SAFETY: republishing the descriptor in the available ring.
            unsafe {
                let ring = (rings + queue.avail_ring_offset(last_used) as u64) as *mut u16;
                ring.write_volatile(id as u16);
                let idx = (rings + queue.avail_idx_offset() as u64) as *mut u16;
                idx.write_volatile(idx.read_volatile().wrapping_add(1));
            }
            last_used = last_used.wrapping_add(1);
        }
        // SAFETY: tell the device there are buffers again, then re-enable the line.
        unsafe {
            write32(mmio, reg::QUEUE_NOTIFY, 0);
            syscall1(SYS_IRQ_ACK, u64::from(irq_cap));
        }
    }

    puts("[inputsrv] input driver exiting\n");
    exit();
}

/// Publish one decoded event to whoever holds the receiving end.
///
/// The send is deliberately allowed to block. An endpoint's ring is small, and a
/// consumer that has stopped draining is a consumer that will lose events either
/// way — but a driver that *drops* them silently produces a keyboard which
/// occasionally misses a keystroke, which is the hardest class of bug there is to
/// believe. Blocking makes the back-pressure visible instead: the driver stops
/// acknowledging interrupts, the device's queue fills, and the failure has a shape.
fn publish(tag: u64, code: u64, value: u64) {
    let mut msg = Message::new();
    msg.tag = tag;
    msg.words[0] = code;
    msg.words[1] = value;
    // SAFETY: `Send` reads one `Message` through this pointer; the endpoint handle
    // is the capability the kernel installed for exactly this.
    unsafe {
        let _ = syscall2(SYS_SEND, EP_EVENTS, core::ptr::addr_of!(msg) as u64);
    }
}

/// Read one event out of the buffer descriptor `id` names, if the device wrote a
/// whole one.
fn read_event(buffers: u64, id: u32, len: u32) -> Option<InputEvent> {
    if id >= u32::from(QUEUE_SIZE) {
        return None;
    }
    let base = buffers + u64::from(id) * EVENT_BUF_BYTES as u64;
    let len = (len as usize).min(EVENT_BUF_BYTES);
    // SAFETY: `base` is one of our own event buffers, inside the DMA run, and
    // `len` is clamped to its size.
    let bytes = unsafe { core::slice::from_raw_parts(base as *const u8, len) };
    InputEvent::parse(bytes)
}

/// Receive one capability from the device manager.
fn recv_cap() -> Option<u32> {
    let mut msg = Message::new();
    // SAFETY: `Recv` writes one `Message` through this pointer.
    let rc = unsafe { syscall2(SYS_RECV, EP_MANAGER, core::ptr::addr_of_mut!(msg) as u64) };
    if rc < 0 || msg.cap == 0 {
        return None;
    }
    Some(msg.cap)
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

/// Write a decimal number.
fn put_dec(mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    // SAFETY: as `puts`; the slice is ASCII digits inside `buf`.
    unsafe {
        let _ = syscall2(
            SYS_DEBUG_WRITE,
            buf[i..].as_ptr() as u64,
            (buf.len() - i) as u64,
        );
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

/// Nothing here panics deliberately; the lang item has to exist regardless.
#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    exit()
}
