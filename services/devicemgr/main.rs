//! The device manager — the first user-space program that is *real Rust*, not
//! naked assembly.
//!
//! It is a separately compiled EL0 program, like `init`, but where `init` is one
//! hand-written `_start` that speaks the syscall ABI directly, this links against
//! the **same `fdt` crate the kernel uses** and parses the device tree on the
//! user side. That is the point of the microkernel: discovering the machine is
//! policy, and policy belongs in user space. The kernel maps the tree read-only
//! into this process (`AddressSpace::map_dtb`) and hands it a single *device
//! authority* capability; the manager finds the UART in the tree and mints a
//! device and an interrupt capability for the address *it* discovered, with
//! `GrantDevice`/`GrantIrq` — the kernel never chose that address.
//!
//! Built by the kernel's `build.rs` with a plain `rustc`, using `fdt` compiled to
//! an rlib and this program's own linker script (the init image's — it, too,
//! links at `USER_BASE`, in its own address space). It needs no `build-std`: at
//! `-Copt-level=2` the compiler inlines the few slice operations `fdt` performs,
//! so `core`'s memory intrinsics are never referenced and the target's
//! pre-compiled `core` suffices.
#![no_std]
#![no_main]

use core::arch::{asm, naked_asm};
use core::panic::PanicInfo;

use staros_cpio::Archive;
use staros_fdt::Fdt;

/// The kernel-seeded data page: id at `+0`, DTB user address at `+8`, DTB length
/// at `+16`, initramfs user address at `+24`, initramfs length at `+32` (see
/// `AddressSpace::write_dtb_info` / `write_initrd_info`).
const USER_DATA_VA: u64 = 0x4_0000_0000;

/// The file the demo reads out of the initramfs to prove user-space unpacking.
const GREETING_FILE: &str = "greeting.txt";

/// An ELF *file* in the initramfs, started with `SpawnImage`. Optional: an archive
/// without it simply skips that half, since the smoke matrix builds the archive
/// only when `cpio` exists on the host.
const PROGRAM_FILE: &str = "init.elf";

/// The id the loaded-from-a-file process is seeded with. Nothing else uses it, so
/// the line it prints can only have come from this path.
const PROGRAM_ID: u64 = 12;

// Syscall numbers — must match `staros_abi::syscall::Syscall`.
const SYS_SEND: usize = 1;
const SYS_EXIT: usize = 4;
const SYS_DEBUG_WRITE: usize = 19;
const SYS_GRANT_DEVICE: usize = 12;
const SYS_GRANT_IRQ: usize = 13;
const SYS_CREATE_DMA: usize = 16;
const SYS_BIND_DMA: usize = 18;
const SYS_SPAWN_IMAGE: usize = 26;
const SYS_MAP_MEMORY: usize = 3;

/// Virtio-mmio register offsets and values we need to *identify* a device. The
/// full map lives in `staros_virtio`, which this program does not link — three
/// constants are cheaper than a second rlib in the build for a program that only
/// reads two registers.
const VIRTIO_MAGIC: u64 = 0x000;
const VIRTIO_DEVICE_ID: u64 = 0x008;
const VIRTIO_MAGIC_VALUE: u32 = 0x7472_6976; // 'virt'
const VIRTIO_DEVICE_ID_INPUT: u32 = 18;

/// The error the kernel returns from `BindDma` on a machine with no IOMMU
/// (`KError::NotSupported`), so the demo can tell "no SMMU here" from a real
/// failure and still run on plain `virt`.
const E_NOT_SUPPORTED: isize = -7;

/// The StreamID we confine. With no real bus master in QEMU this is illustrative:
/// PCIe RID 00:00.0 maps to StreamID 0 under the `virt` machine's `iommu-map`.
const DEMO_STREAM_ID: u64 = 0;
/// Pages in the DMA buffer we bind (matches the DMA demo elsewhere: 4 × 4 KiB).
const DMA_PAGES: u64 = 4;

/// Our capability table, as `main.rs` granted it: the device authority, and two
/// send endpoints on which we delegate what we mint.
const AUTHORITY: u64 = 1;
const EP_DRIVER: u64 = 2;
const EP_SERVER: u64 = 3;
/// The endpoint the input driver receives its device and interrupt on.
const EP_INPUT: u64 = 4;

/// A message as the IPC ABI lays it out (`staros_ipc::Message`): tag, payload
/// words, and a capability handle to transfer. `#[repr(C)]` so the field offsets
/// match what the kernel reads.
#[repr(C)]
struct Msg {
    tag: u64,
    words: [u64; 4],
    cap: u32,
    _pad: u32,
}

/// The interrupt-cell count for a GIC (`#interrupt-cells = <3>`): kind, number,
/// flags. Kind 0 is an SPI (shared peripheral, numbered from 32); kind 1 a PPI.
const GIC_INTERRUPT_CELLS: u32 = 3;
/// First SPI interrupt id — an SPI numbered `n` in the tree is GIC intid `32 + n`.
const SPI_BASE: u32 = 32;

/// Entry point, forced to offset 0 (`e_entry`) by the linker script. The kernel
/// has already set `SP_EL0`, so we jump straight into Rust with a working stack.
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.start"]
extern "C" fn _start() -> ! {
    naked_asm!("b {main}", main = sym devicemgr_main);
}

extern "C" fn devicemgr_main() -> ! {
    // The kernel mapped the tree read-only and told us where. Read the address and
    // length it seeded, then hand `fdt` exactly that slice.
    // SAFETY: the kernel seeded these two words in our data page before our TTBR0
    // ran, and mapped `[dtb_va, dtb_va + len)` read-only into this space.
    let dtb_va = unsafe { ((USER_DATA_VA + 8) as *const u64).read_volatile() };
    let dtb_len = unsafe { ((USER_DATA_VA + 16) as *const u32).read_volatile() } as usize;

    // SAFETY: the mapping above backs this slice; `Fdt::new` validates the header
    // and never reads past `dtb_len`.
    let blob = unsafe { core::slice::from_raw_parts(dtb_va as *const u8, dtb_len) };
    let Ok(fdt) = Fdt::new(blob) else {
        puts("[devicemgr] device tree did not parse in user space\n");
        exit();
    };

    // Find the UART the same way the kernel did — but here, in an EL0 process.
    let Some((uart_phys, uart_intid)) = find_pl011(&fdt) else {
        puts("[devicemgr] no PL011 in the device tree\n");
        exit();
    };

    puts("[devicemgr] parsed the device tree in user space: PL011 @ 0x");
    put_hex(uart_phys);
    puts(" intid ");
    put_dec(u64::from(uart_intid));
    puts("\n");

    // Mint what the driver needs — a device capability for the UART and its
    // interrupt — plus a *separate* device object for the server (a distinct
    // object so the server revoking its own does not disturb the driver's). All
    // from the authority, for the address and line we discovered.
    // SAFETY: plain syscalls; the kernel validates the authority and arguments.
    let dev_driver = unsafe { syscall2(SYS_GRANT_DEVICE, AUTHORITY, uart_phys) };
    let irq_driver = unsafe { syscall2(SYS_GRANT_IRQ, AUTHORITY, u64::from(uart_intid)) };
    let dev_server = unsafe { syscall2(SYS_GRANT_DEVICE, AUTHORITY, uart_phys) };
    if dev_driver < 0 || irq_driver < 0 || dev_server < 0 {
        puts("[devicemgr] a grant was refused\n");
        exit();
    }

    // Delegate them over IPC: the driver gets its device then its interrupt, the
    // server gets its device. From here the driver and server are exactly as
    // capable as before — but their authority came from a user-space manager that
    // read the tree, not from the kernel's `main`.
    send_cap(EP_DRIVER, dev_driver as u32);
    send_cap(EP_DRIVER, irq_driver as u32);
    send_cap(EP_SERVER, dev_server as u32);
    puts("[devicemgr] delegated UART device+irq to the driver and a device to the server\n");

    // The input device, if this machine has one. Same shape as the UART: find it,
    // mint a device and an interrupt capability, delegate both. The difference is
    // that finding it needed a *look* at each slot's registers rather than a
    // property in the tree — see `find_virtio_input`.
    match find_virtio_input(&fdt) {
        Some((phys, intid)) => {
            // SAFETY: minting from the authority we hold, as above.
            let (dev, irq) = unsafe {
                (
                    syscall2(SYS_GRANT_DEVICE, AUTHORITY, phys),
                    syscall2(SYS_GRANT_IRQ, AUTHORITY, u64::from(intid)),
                )
            };
            if dev < 0 || irq < 0 {
                puts("[devicemgr] could not mint capabilities for the input device\n");
            } else {
                send_cap(EP_INPUT, dev as u32);
                send_cap(EP_INPUT, irq as u32);
                puts("[devicemgr] found a virtio-input device at ");
                put_hex(phys);
                puts(" intid ");
                put_dec(u64::from(intid));
                puts(" and delegated it to the input driver\n");
            }
        }
        None => puts("[devicemgr] no virtio-input device on this machine\n"),
    }

    // Enforcement (roadmap 2.3): make DMA safe against a bus master. A driver we
    // trust with a DMA-capable device could, without an IOMMU, point that device
    // at kernel memory. So we create a DMA buffer and bind it to the device's
    // StreamID at the SMMU — after which the device may reach only these pages.
    // The authority gates this exactly as it gates minting: we hold it, so we may.
    // SAFETY: plain syscalls; the kernel validates the authority, the DMA handle,
    // and the StreamID.
    let dma = unsafe { syscall1(SYS_CREATE_DMA, DMA_PAGES) };
    if dma < 0 {
        puts("[devicemgr] could not allocate a DMA buffer\n");
        exit();
    }
    // SAFETY: authority in x0, DMA handle in x1, StreamID in x2.
    let bound = unsafe { syscall3(SYS_BIND_DMA, AUTHORITY, dma as u64, DEMO_STREAM_ID) };
    if bound == 0 {
        puts("[devicemgr] bound the DMA buffer to StreamID ");
        put_dec(DEMO_STREAM_ID);
        puts(" at the SMMU - the device is confined to its buffer, kernel memory is unreachable\n");
    } else if bound == E_NOT_SUPPORTED {
        puts("[devicemgr] no IOMMU on this machine; DMA capability stands but is unenforced\n");
    } else {
        puts("[devicemgr] the SMMU refused the stream binding\n");
    }

    // Unpack the initramfs, if the bootloader left one. This is a filesystem with
    // zero storage-driver code: the kernel mapped the CPIO archive read-only and
    // told us where; we parse it in user space and read a file out of it (roadmap
    // 2.4). The bytes we print came from a file on the build host, passed via
    // `-initrd`, not baked into any binary — that is what makes this falsifiable.
    unpack_initramfs();

    exit();
}

/// Read the initramfs info the kernel seeded, parse the CPIO archive, and print a
/// known file's contents — proof that a user-space process turned a raw blob into
/// files with no filesystem driver in the kernel.
fn unpack_initramfs() {
    // SAFETY: the kernel seeded these two words (initrd VA at +24, length at +32)
    // before our TTBR0 ran, and mapped `[initrd_va, initrd_va + len)` read-only.
    let initrd_va = unsafe { ((USER_DATA_VA + 24) as *const u64).read_volatile() };
    let initrd_len = unsafe { ((USER_DATA_VA + 32) as *const u32).read_volatile() } as usize;

    if initrd_len == 0 {
        puts("[devicemgr] no initramfs on this machine\n");
        return;
    }

    // SAFETY: the mapping above backs this slice; the parser never reads past
    // `initrd_len` and treats malformed bytes as end-of-archive.
    let blob = unsafe { core::slice::from_raw_parts(initrd_va as *const u8, initrd_len) };
    let archive = Archive::new(blob);

    // Count the members, then read one file out by name.
    let count = archive.entries().count();
    puts("[devicemgr] unpacked the initramfs in user space: ");
    put_dec(count as u64);
    puts(" files, no storage driver\n");

    match archive.find(GREETING_FILE) {
        Some(entry) => {
            puts("[devicemgr] read '");
            puts(GREETING_FILE);
            puts("' from the initramfs: ");
            // Print the file's bytes verbatim (it ends in its own newline, which
            // flushes the whole line atomically).
            for &b in entry.data {
                out_byte(b);
            }
        }
        None => {
            puts("[devicemgr] initramfs has no '");
            puts(GREETING_FILE);
            puts("'\n");
        }
    }

    // And the other half: a *program* out of the same archive. We hand the kernel
    // the bytes we found; it parses the ELF and builds an address space. Nothing in
    // the kernel knows this came from a CPIO archive — the parsing happened here,
    // in EL0, with no filesystem anywhere.
    if let Some(entry) = archive.find(PROGRAM_FILE) {
        // SAFETY: `svc` with the SpawnImage convention; the kernel walks our own
        // page tables before reading a byte of the buffer we name.
        let rc = unsafe {
            syscall3(
                SYS_SPAWN_IMAGE,
                entry.data.as_ptr() as u64,
                entry.data.len() as u64,
                PROGRAM_ID,
            )
        };
        if rc < 0 {
            puts("[devicemgr] the kernel refused the program from the initramfs\n");
            return;
        }
        puts("[devicemgr] started '");
        puts(PROGRAM_FILE);
        puts("' from the initramfs as a new process - the kernel loaded a file, not a built-in image\n");

        // Two things the kernel must refuse, checked here because this is the only
        // place holding bytes to refuse. Both are what a loader gets handed by
        // mistake sooner or later: a file that is not a program, and a pointer that
        // is not memory.
        let not_a_program = archive.find(GREETING_FILE).map_or(0, |e| {
            // SAFETY: as above.
            unsafe {
                syscall3(
                    SYS_SPAWN_IMAGE,
                    e.data.as_ptr() as u64,
                    e.data.len() as u64,
                    PROGRAM_ID,
                )
            }
        });
        // An address inside our own window that nothing maps. A range check could
        // not catch this — the kernel has to walk our tables before it reads.
        // SAFETY: as above; the kernel never dereferences this pointer.
        let not_mapped = unsafe { syscall3(SYS_SPAWN_IMAGE, 0x8040_0000, 64, PROGRAM_ID) };
        if not_a_program < 0 && not_mapped < 0 {
            puts("[devicemgr] the kernel refused a non-ELF file and an unmapped pointer, as it must\n");
        } else {
            puts("[devicemgr] SPAWNIMAGE WRONG - the kernel accepted a non-program or an unmapped pointer\n");
        }
    }
}

/// Send a capability (by handle) over endpoint `ep`, transferring nothing else.
fn send_cap(ep: u64, cap: u32) {
    let mut msg = Msg { tag: 0, words: [0; 4], cap, _pad: 0 };
    // SAFETY: `Send` reads a `Message` at the pointer; `Msg` matches its layout,
    // and `ep` is a send-capable endpoint handle.
    unsafe { syscall2(SYS_SEND, ep, (&raw mut msg) as u64) };
}

/// Find the virtio-mmio slot that actually holds an input device, and return its
/// physical base and interrupt id.
///
/// QEMU's `virt` machine declares **thirty-two** identical `virtio,mmio` nodes and
/// leaves almost all of them empty; which one is populated depends on the order
/// `-device` arguments were given. So the tree cannot answer this on its own: the
/// only way to tell is to look at each slot's `DeviceID` register, which means
/// mapping it. That is exactly what a device manager is for — it holds the
/// authority to mint a capability for any page, so it can look where a driver may
/// not, and hand on only the one slot that matters.
fn find_virtio_input(fdt: &Fdt<'_>) -> Option<(u64, u32)> {
    for node in fdt.find_all_compatible("virtio,mmio") {
        let Some((phys, _)) = node.reg().and_then(|mut r| r.next()) else {
            continue;
        };
        let Some((kind, number, _)) = node
            .interrupts(GIC_INTERRUPT_CELLS)
            .and_then(|mut i| i.next())
        else {
            continue;
        };
        let intid = if kind == 0 { SPI_BASE + number } else { number };

        // Mint ourselves a capability for this slot and map it. The mapping goes
        // to the same fixed address every time, so each slot is inspected and
        // replaced — we are looking, not keeping.
        // SAFETY: `GrantDevice` mints from the authority we hold; `MapMemory` maps
        // the page the capability names into our own space.
        let va = unsafe {
            let cap = syscall2(SYS_GRANT_DEVICE, AUTHORITY, phys);
            if cap < 0 {
                continue;
            }
            syscall1(SYS_MAP_MEMORY, cap as u64)
        };
        if va < 0 {
            continue;
        }
        // SAFETY: the page is mapped Device memory, read-only here; these two
        // registers exist on every virtio-mmio transport, populated or not.
        let (magic, device_id) = unsafe {
            (
                ((va as u64 + VIRTIO_MAGIC) as *const u32).read_volatile(),
                ((va as u64 + VIRTIO_DEVICE_ID) as *const u32).read_volatile(),
            )
        };
        if magic == VIRTIO_MAGIC_VALUE && device_id == VIRTIO_DEVICE_ID_INPUT {
            return Some((phys, intid));
        }
    }
    None
}

/// Find the first PL011 UART in the tree and return its physical base and GIC
/// interrupt id, decoded from the node's `reg` and `interrupts` properties.
fn find_pl011(fdt: &Fdt<'_>) -> Option<(u64, u32)> {
    let node = fdt.find_all_compatible("arm,pl011").next()?;
    let (phys, _) = node.reg()?.next()?;
    let (kind, number, _) = node.interrupts(GIC_INTERRUPT_CELLS)?.next()?;
    // Kind 0 = SPI (numbered from 32); anything else we do not expect for a UART.
    let intid = if kind == 0 { SPI_BASE + number } else { number };
    Some((phys, intid))
}

/// A single output line, accumulated so it can be emitted in one atomic write.
///
/// The device manager builds each log line from several `puts`/`put_dec`/`put_hex`
/// calls. Sending those byte-by-byte (the old `DebugPutc` loop) let two cores
/// shred each other's lines into "OhMeMlUl"; worse on the framebuffer, where every
/// byte also moves a shared cursor. So bytes are buffered here and the whole line
/// is flushed with a single `DebugWrite` on its terminating newline — one hold of
/// the kernel console lock per line, so a line is never interleaved with another
/// core's output.
///
/// This process is a single EL0 task: its code runs on one core at a time, so the
/// one static buffer below is never touched concurrently.
struct LineBuf {
    buf: [u8; 512],
    len: usize,
}

static mut LINE: LineBuf = LineBuf { buf: [0; 512], len: 0 };

impl LineBuf {
    /// Append one byte, flushing the accumulated line when it completes (`\n`) or
    /// the buffer is full.
    fn byte(&mut self, b: u8) {
        if self.len < self.buf.len() {
            self.buf[self.len] = b;
            self.len += 1;
        }
        if b == b'\n' || self.len == self.buf.len() {
            self.flush();
        }
    }

    /// Emit whatever is buffered as one atomic `DebugWrite`, then reset.
    fn flush(&mut self) {
        if self.len == 0 {
            return;
        }
        // SAFETY: `DebugWrite` reads `len` bytes at the pointer and needs no
        // capability; the slice is a live stack/static buffer of exactly that len.
        unsafe { syscall2(SYS_DEBUG_WRITE, self.buf.as_ptr() as u64, self.len as u64) };
        self.len = 0;
    }
}

/// Append one byte to the current output line (see [`LineBuf`]).
fn out_byte(b: u8) {
    // SAFETY: single-task process, so `LINE` is never accessed concurrently.
    // `addr_of_mut!` takes the `&mut` without forming a reference to the `static
    // mut` directly (avoids the `static_mut_refs` lint).
    unsafe { (*core::ptr::addr_of_mut!(LINE)).byte(b) };
}

/// Flush any partially-built line. Called before exit so a final line with no
/// trailing newline is not lost.
fn flush_line() {
    // SAFETY: as `out_byte`.
    unsafe { (*core::ptr::addr_of_mut!(LINE)).flush() };
}

/// Print a string to the debug console. Buffered into the current line and emitted
/// atomically when the line ends, so it is never interleaved mid-line with another
/// core's output.
fn puts(s: &str) {
    for &b in s.as_bytes() {
        out_byte(b);
    }
}

/// Print `v` as lower-case hex with no leading zeros (except for zero itself).
fn put_hex(v: u64) {
    if v == 0 {
        out_byte(b'0');
        return;
    }
    let mut buf = [0u8; 16];
    let mut n = 0;
    let mut x = v;
    while x != 0 {
        let d = (x & 0xf) as u8;
        buf[n] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
        n += 1;
        x >>= 4;
    }
    while n > 0 {
        n -= 1;
        out_byte(buf[n]);
    }
}

/// Print `v` as decimal.
fn put_dec(v: u64) {
    if v == 0 {
        out_byte(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut n = 0;
    let mut x = v;
    while x != 0 {
        buf[n] = b'0' + (x % 10) as u8;
        n += 1;
        x /= 10;
    }
    while n > 0 {
        n -= 1;
        out_byte(buf[n]);
    }
}

/// A one-argument syscall: number in x8, arg in x0, result in x0.
///
/// # Safety
/// Issues `svc #0`; the caller must pass a valid syscall number and argument.
#[inline]
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
#[inline]
unsafe fn syscall2(number: usize, a0: u64, a1: u64) -> isize {
    let ret;
    // SAFETY: as `syscall1`, with a second argument in x1.
    unsafe {
        asm!("svc #0", in("x8") number, inout("x0") a0 => ret, in("x1") a1, options(nostack));
    }
    ret
}

/// A three-argument syscall: number in x8, args in x0/x1/x2, result in x0.
///
/// # Safety
/// As [`syscall1`].
#[inline]
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

/// Terminate this process.
fn exit() -> ! {
    // Emit any line that has no trailing newline before we go, so it is not lost.
    flush_line();
    // SAFETY: `Exit` never returns; the kernel tears the task down.
    unsafe { syscall1(SYS_EXIT, 0) };
    loop {}
}

/// Required for `no_std` linkage; the program never panics at runtime.
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}
