//! The `init` boot image — the first user-space program, as a *separately
//! compiled* EL0 binary that the kernel loads at runtime.
//!
//! This is deliberately **not** part of the Cargo workspace build. The kernel's
//! `build.rs` compiles this single file with `rustc` for `aarch64-unknown-none`
//! using [`image.ld`](image.ld) (which links it at `USER_BASE` = 0x8000_0000
//! with separate read-execute and read-write `PT_LOAD` segments). The kernel
//! `include_bytes!`s the resulting **ELF** and its ELF loader parses the program
//! headers, mapping each segment at its own VA with its own rights, then enters
//! it at `e_entry` — so what runs is a real, independently-linked user program,
//! not naked assembly baked into the kernel's own `.text`.
//!
//! Because the loader maps each segment at exactly its link address, the code may
//! use absolute VAs freely; it is nonetheless written as one naked `_start` so
//! it needs no runtime, no relocations, and no `core` memory intrinsics (hence
//! no `build-std`). Its inputs are the ABI syscall numbers (kept in sync with
//! `staros_abi::syscall`) and the per-process id byte the kernel seeds at
//! `USER_DATA_VA`. It also carries a tiny `.data`/`.bss` pair the server touches
//! to prove the loader mapped its writable segment correctly.
//!
//! The program is the capability / cross-task-revocation demo: branch on this
//! process's id into a *client* (id 1, holds no device authority) or a *resource
//! server* (id 2, holds the UART capability). The server replies to the client's
//! request by **delegating** a copy of its UART capability, then drives the UART
//! itself and **revokes the UART object globally** before exiting. The client
//! receives the delegated capability — but by the time it tries to map the UART,
//! the object has been revoked by the server, so the kernel denies it: proof that
//! revoking an object invalidates every holder's capability at once, across tasks.
#![no_std]
#![no_main]

use core::arch::naked_asm;
use core::panic::PanicInfo;

/// Entry point. Linked (and loaded) at `USER_BASE` = 0x8000_0000 and named by the
/// ELF's `e_entry`, so the kernel `eret`s straight to it. Pure position-correct
/// assembly — see the module docs for the protocol it speaks.
///
/// The kernel builds this space's page tables lazily, so the three regions this
/// program touches — its image at `USER_BASE`, its id byte at `USER_DATA_VA` and
/// its stack near `USER_STACK_TOP` — are gigabytes apart and cost only the tables
/// that actually reach them.
///
/// Syscall numbers (must match `staros_abi::syscall::Syscall`):
/// Send=1, Recv=2, MapMemory=3, Exit=4, DebugPutc=5, Revoke=6, DebugWrite=19.
/// Whole lines are printed with `DebugWrite` via the `.Lputs` helper (one atomic
/// syscall per line); `DebugPutc` is kept only for the odd lone byte. The
/// per-process id lives at
/// `USER_DATA_VA` = 0x4_0000_0000 — far above the image, which is free to grow up
/// to `USER_IMAGE_END` now that the EL0 window is a real sparse address space.
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.start"]
extern "C" fn _start() -> ! {
    naked_asm!(
        "movz x9, #0x4, lsl #32",    // x9 = USER_DATA_VA (0x4_0000_0000)
        "ldrb w19, [x9]",            // w19 = process id
        "sub sp, sp, #64",           // reserve a Message buffer on the user stack
        "mov x11, sp",               // x11 = &msg
        "cmp w19, #1",
        "b.ne 4f",                   // id != 1 -> server

        // ================= client (id 1) =================
        // The client holds *no* device authority. It asks the server for the
        // UART by sending a request, then tries to use the capability the server
        // delegates back — and discovers the server has already revoked it.
        //
        // Build a request message: tag = 1, no payload, no capability.
        "stp xzr, xzr, [x11]",
        "stp xzr, xzr, [x11, #16]",
        "stp xzr, xzr, [x11, #32]",
        "mov x0, #1",
        "str x0, [x11]",             // msg.tag = 1
        // Send it three times on ep0 (handle 1). The ring holds 2, so the 3rd
        // Send blocks until the server drains a slot — exercising blocking send
        // and handing the CPU to the server, which runs to completion (reply +
        // revoke + exit) before this client is scheduled again.
        "mov x0, #1",
        "mov x1, x11",
        "mov x8, #1",                // Syscall::Send
        "svc #0",
        "mov x0, #1",
        "mov x1, x11",
        "mov x8, #1",                // Syscall::Send
        "svc #0",
        "mov x0, #1",
        "mov x1, x11",
        "mov x8, #1",                // Syscall::Send (blocks: ring full)
        "svc #0",
        // Receive the reply on ep1 (handle 2); the kernel installs the delegated
        // UART capability into our table and writes its new handle to msg.cap.
        "mov x0, #2",
        "mov x1, x11",
        "mov x8, #2",                // Syscall::Recv
        "svc #0",
        "ldr w21, [x11, #40]",       // w21 = handle of the delegated UART capability
        // Receive the second reply: the shared-memory capability. Map it and read
        // the marker the SERVER wrote — from our own mapping of the same physical
        // page. Printing those bytes proves the two tasks share memory.
        "mov x0, #2",
        "mov x1, x11",
        "mov x8, #2",                // Syscall::Recv
        "svc #0",
        "ldr w22, [x11, #40]",       // w22 = shared capability handle
        "mov x0, x22",
        "mov x8, #15",               // Syscall::MapShared
        "svc #0",
        "mov x23, x0",               // x23 = shared VA in our space
        "adr x2, 10f",               // "[client] read from shared memory: "
        "bl .Lputs",
        "mov x2, x23",               // the NUL-terminated bytes the server left there
        "bl .Lputs",
        "mov w0, #0x0a",             // trailing newline (a lone byte: DebugPutc is fine)
        "mov x8, #5",                // Syscall::DebugPutc
        "svc #0",
        // Try to use the delegated capability. By now the server has revoked the
        // UART *object* globally, so this MapMemory is denied by the kernel even
        // though the client still holds a handle to it — cross-task revocation.
        "mov x0, x21",
        "mov x8, #3",                // Syscall::MapMemory -> denied (kernel logs)
        "svc #0",
        // Hand the kernel a syscall pointer into a page nothing has mapped. It
        // must come back as an error: the kernel walks *our* page tables before it
        // dereferences anything we hand it, so a bad pointer cannot turn into a
        // data abort inside the kernel. (A plain range check could not catch this
        // — the address is well inside the region our image lives in.)
        "movz x1, #0x8040, lsl #16", // 0x8040_0000: past our 3 MiB image, unmapped
        "mov x0, #1",                // ep0 — a capability we really do hold
        "mov x8, #1",                // Syscall::Send
        "svc #0",
        "cmp x0, #0",
        "b.ge .Lcl_exit",            // accepted?! the guard is broken; say nothing
        "adr x2, 6f",
        "bl .Lputs",
        ".Lcl_exit:",
        "mov x8, #4",                // Syscall::Exit
        "svc #0",

        // ================= server (id 2) =================
        "4:",
        "cmp w19, #2",
        "b.ne 5f",                   // id != 2 -> UART-RX interrupt driver
        // First receive the UART device capability the device manager delegates on
        // ep_srv (handle 3). We hold no device authority of our own now — it is
        // granted to us, from a manager that read the tree.
        "mov x0, #3",                // ep_srv handle (recv)
        "mov x1, x11",
        "mov x8, #2",                // Syscall::Recv
        "svc #0",
        "ldr w23, [x11, #40]",       // w23 = handle of the delegated UART device cap
        // Receive the client's request on ep0 (handle 1).
        "mov x0, #1",
        "mov x1, x11",
        "mov x8, #2",                // Syscall::Recv
        "svc #0",
        // Build the reply: tag = 2, words[0] = '>', cap = our UART handle (w23).
        "stp xzr, xzr, [x11]",
        "stp xzr, xzr, [x11, #16]",
        "stp xzr, xzr, [x11, #32]",
        "mov x0, #2",
        "str x0, [x11]",             // msg.tag = 2
        "mov w0, #62",               // '>'
        "str x0, [x11, #8]",         // msg.words[0] = '>'
        "str w23, [x11, #40]",       // msg.cap = delegated UART handle
        // Send the reply on ep1 (handle 2) — this delegates a copy of the UART
        // capability to the client.
        "mov x0, #2",
        "mov x1, x11",
        "mov x8, #1",                // Syscall::Send
        "svc #0",
        // Create a shared-memory buffer, write a marker string into it, and
        // delegate it to the client on ep1. The client will read these very bytes
        // from its OWN mapping of the same physical page — payload by reference,
        // not copied through six message words.
        "mov x8, #14",               // Syscall::CreateShared
        "svc #0",
        "mov x25, x0",               // x25 = shared capability handle
        "mov x0, x25",
        "mov x8, #15",               // Syscall::MapShared
        "svc #0",
        "mov x26, x0",               // x26 = shared VA in our space
        "adr x2, 9f",                // marker string to place in shared memory
        ".Lsrv_shwrite:",
        "ldrb w0, [x2], #1",
        "strb w0, [x26], #1",        // copy into shared memory (incl. NUL)
        "cbnz w0, .Lsrv_shwrite",
        // Send a second reply on ep1 delegating the shared capability.
        "stp xzr, xzr, [x11]",
        "stp xzr, xzr, [x11, #16]",
        "stp xzr, xzr, [x11, #32]",
        "str w25, [x11, #40]",       // msg.cap = shared handle
        "mov x0, #2",                // ep1
        "mov x1, x11",
        "mov x8, #1",                // Syscall::Send
        "svc #0",
        // Prove the ELF loader placed our writable segment correctly: read the
        // initialized '.data' marker, stash it through '.bss' (which must be
        // writable), and read it back. A mis-mapped segment would fault here or
        // yield the wrong byte — so seeing this marker on the console means the
        // loader honoured both the R-X and R-W program headers.
        "adrp x3, {marker}",
        "add x3, x3, :lo12:{marker}",
        "ldrb w22, [x3]",            // w22 = DATA_MARKER from .data (R-W segment)
        "adrp x5, {scratch}",
        "add x5, x5, :lo12:{scratch}",
        "strb w22, [x5]",            // write into .bss (must be writable)
        "ldrb w22, [x5]",            // read it back
        // Drive the UART ourselves via the delegated cap (w23), then announce and
        // revoke it.
        "mov x0, x23",
        "mov x8, #3",                // Syscall::MapMemory
        "svc #0",
        "mov x10, x0",               // x10 = UART VA
        "strb w22, [x10]",           // print the .data/.bss marker first
        "adr x2, 8f",                // baked greeting
        "2:",
        "ldrb w0, [x2], #1",
        "cbz w0, 3f",
        "strb w0, [x10]",            // MMIO via our own UART capability
        "b 2b",
        "3:",
        // Revoke the UART *object* globally. Every capability to it — including
        // the copy we just delegated to the client — stops resolving at once.
        "mov x0, x23",
        "mov x8, #6",                // Syscall::Revoke
        "svc #0",
        "mov x8, #4",                // Syscall::Exit
        "svc #0",

        // ============ UART-RX interrupt driver (id 3) ============
        // A fully user-space driver: it owns the UART MMIO (via MapMemory) *and*
        // its interrupt line. It sleeps in `Wait` until the kernel forwards the
        // UART RX interrupt, then drains and echoes the received bytes — the
        // kernel never touches the device or its FIFO.
        "5:",
        "cmp w19, #3",
        "b.ne .Lcanary",             // id != 3 -> canary (deliberate faulting task)
        // Receive our UART device capability, then our interrupt capability, both
        // delegated by the device manager on ep_drv (handle 1). Nothing about the
        // device is baked in here — it arrives over IPC from a manager that read
        // the tree.
        "mov x0, #1",                // ep_drv handle (recv)
        "mov x1, x11",
        "mov x8, #2",                // Syscall::Recv
        "svc #0",
        "ldr w23, [x11, #40]",       // w23 = delegated UART device cap handle
        "mov x0, #1",
        "mov x1, x11",
        "mov x8, #2",                // Syscall::Recv
        "svc #0",
        "ldr w24, [x11, #40]",       // w24 = delegated interrupt cap handle
        // Map the UART through the delegated device capability.
        "mov x0, x23",
        "mov x8, #3",                // Syscall::MapMemory
        "svc #0",
        "mov x10, x0",               // x10 = UART VA
        // Enable the UART (UARTEN|TXE|RXE) and unmask the RX interrupt (IMSC.RXIM).
        "movz w0, #0x301",
        "str w0, [x10, #0x30]",      // UARTCR
        "mov w0, #0x10",
        "str w0, [x10, #0x38]",      // UARTIMSC: RXIM (bit 4)
        // Register the delegated interrupt capability (w24); the kernel enables
        // the line and returns a notification handle for it.
        "mov x0, x24",
        "mov x8, #7",                // Syscall::IrqRegister
        "svc #0",
        "mov x21, x0",               // x21 = notification handle
        // Banner, so the log shows the driver reached its wait loop.
        "adr x2, 1f",                // driver banner string
        ".Ldrv_banner:",
        "ldrb w0, [x2], #1",
        "cbz w0, .Ldrv_wait",
        "str w0, [x10]",
        "b .Ldrv_banner",
        // Service loop: block until the UART interrupt is forwarded, then drain.
        ".Ldrv_wait:",
        "mov x0, x21",
        "mov x8, #8",                // Syscall::Wait (blocks until UART RX fires)
        "svc #0",
        ".Ldrv_drain:",
        "ldr w1, [x10, #0x18]",      // UARTFR
        "tst w1, #0x10",             // RXFE: RX FIFO empty?
        "b.ne .Ldrv_drained",        // empty -> done draining this burst
        "ldr w0, [x10]",             // UARTDR: read a byte (clears the source)
        "and w2, w0, #0xff",
        "str w2, [x10]",             // echo it back to the console
        "cmp w2, #0x0a",             // newline -> finish the demo
        "b.eq .Ldrv_finish",
        "b .Ldrv_drain",
        ".Ldrv_drained:",
        // Clear the UART's RX interrupt latch, then ack so the kernel re-enables
        // the line (it masked it on the way in) for the next byte.
        "mov w0, #0x10",
        "str w0, [x10, #0x44]",      // UARTICR: RXIC
        "mov x0, x24",               // delegated interrupt cap handle
        "mov x8, #9",                // Syscall::IrqAck
        "svc #0",
        "b .Ldrv_wait",
        ".Ldrv_finish:",
        "adr x2, 2f",                // driver farewell string
        ".Ldrv_bye:",
        "ldrb w0, [x2], #1",
        "cbz w0, .Ldrv_exit",
        "str w0, [x10]",
        "b .Ldrv_bye",
        ".Ldrv_exit:",
        "mov x8, #4",                // Syscall::Exit
        "svc #0",

        // ============ canary (id 4): deliberate EL0 fault ============
        // Holds no capabilities. It reads the physical address of kernel RAM from
        // EL0. Nothing in this space maps it — the kernel lives in TTBR1 and no
        // task's tables mention it — so the read raises a data abort. The kernel
        // isolates the fault: it kills just this task and keeps scheduling the
        // others, rather than halting.
        ".Lcanary:",
        "cmp w19, #4",
        "b.ne .Lmemtest",            // id != 4 -> memtest
        "movz x0, #0x4000, lsl #16", // x0 = 0x4000_0000 (kernel RAM, unmapped here)
        "ldr x1, [x0]",              // EL0 read -> data abort -> kernel kills us
        // Not reached: the kernel terminated this task at the faulting load.
        "mov x8, #4",                // Syscall::Exit
        "svc #0",

        // ============ memtest (id 5): big image + dynamic memory ============
        // Holds no capabilities either. This is the task that leans on the page
        // tables: both halves of it were impossible while an address space was one
        // 2 MiB leaf table with a 32-frame ownership list beside it.
        ".Lmemtest:",
        "cmp w19, #5",
        "b.ne .Lspawner",            // id != 5 -> spawner
        "movz x1, #0xBEEF",          // magic value 0xDEADBEEF
        "movk x1, #0xDEAD, lsl #16",
        // (a) Our own image is bigger than the old window. BIG_BSS is 3 MiB of
        // .bss, so this segment's memsz spans two L2 entries; reach 2.5 MiB into
        // it, which lands in a *different* leaf table than the code we are running.
        "adrp x3, {big}",
        "add x3, x3, :lo12:{big}",
        "movz x4, #0x28, lsl #16",   // + 2.5 MiB
        "add x3, x3, x4",
        "ldr x5, [x3]",              // the loader must have zero-filled this tail
        "cbnz x5, .Lmt_exit",        // not zero -> exit quietly (no report)
        "str x1, [x3]",
        "ldr x2, [x3]",
        "cmp x1, x2",
        "b.ne .Lmt_exit",            // mismatch -> exit quietly (no report)
        // (b) Grow the heap by far more pages than a fixed frame list ever held,
        // touching every one. Each MapAnon may make the kernel build new tables.
        "movz x23, #0x1000",         // target: 4096 pages = 16 MiB
        "mov x24, xzr",              // pages mapped so far
        ".Lmt_grow:",
        "mov x8, #10",               // Syscall::MapAnon
        "svc #0",
        "cmp x0, #0",
        "b.lt .Lmt_exit",            // pool exhausted -> exit quietly (no report)
        "str x1, [x0]",              // write the fresh page
        "ldr x2, [x0]",              // read it back
        "cmp x1, x2",
        "b.ne .Lmt_exit",
        "add x24, x24, #1",
        "cmp x24, x23",
        "b.lo .Lmt_grow",
        // (c) DMA buffer: ask for 4 physically-contiguous, non-cacheable pages,
        // then write a marker into the FIRST and LAST page and read both back.
        // Success proves all four pages are mapped (so the run really is
        // contiguous) and that a non-cacheable store is visible on the matching
        // load — the coherency a real device depends on.
        "mov x0, #4",                // 4 pages
        "mov x8, #16",               // Syscall::CreateDma
        "svc #0",
        "cmp x0, #0",
        "b.lt .Lmt_exit",            // no DMA buffer -> exit quietly (no report)
        "mov x8, #17",               // Syscall::MapDma (x0 = dma handle)
        "svc #0",
        "cmp x0, #0",
        "b.lt .Lmt_exit",
        "mov x25, x0",               // x25 = DMA VA (non-cacheable)
        "str x1, [x25]",             // marker into page 0
        "movz x4, #0x3000",          // 3 * 4096: offset of page 3
        "add x6, x25, x4",
        "str x1, [x6]",              // marker into page 3
        "ldr x2, [x25]",             // read page 0 back
        "cmp x1, x2",
        "b.ne .Lmt_exit",
        "ldr x2, [x6]",              // read page 3 back
        "cmp x1, x2",
        "b.ne .Lmt_exit",
        "adr x2, 11f",               // DMA report string
        "bl .Lputs",
        // Report success over the unprivileged debug console (no capability).
        // Only reached if every page above checked out.
        "adr x2, 3f",
        "bl .Lputs",
        ".Lmt_exit:",
        "mov x8, #4",                // Syscall::Exit
        "svc #0",

        // ============ spawner (id 6): create a child via Spawn ============
        // A minimal `init`-style root: it asks the kernel to create a brand-new
        // EL0 process (id 7) at runtime, then reports. The kernel builds the
        // child's address space from the same image and schedules it.
        ".Lspawner:",
        "cmp w19, #6",
        "b.ne .Lchild",              // id != 6 -> a spawned child (id 8)
        // Six children. The kernel starts this demo with seven tasks of its own,
        // so these push the total to thirteen — well past the old `MAX_TASKS = 8`,
        // where `Spawn` returned failure. Each child announces itself, so the
        // kernel's own `sched::task_count` (printed at the end) is the proof.
        "mov w20, #6",               // children to create
        ".Lsp_loop:",
        "mov x0, #8",                // child id (they all run the same code)
        "mov x8, #11",               // Syscall::Spawn
        "svc #0",
        "subs w20, w20, #1",         // x19/x20 are callee-saved across `svc`
        "b.ne .Lsp_loop",
        "adr x2, 4f",                // spawner report string
        "bl .Lputs",
        "mov x8, #4",                // Syscall::Exit
        "svc #0",

        // ============ child (id 8): spawned at runtime ============
        // This process was not set up by the kernel at boot — the spawner created
        // it with the `Spawn` syscall. It just announces itself and exits.
        ".Lchild:",
        "adr x2, 5f",                // child greeting string
        "bl .Lputs",
        "mov x8, #4",                // Syscall::Exit
        "svc #0",

        // -------- puts: emit a whole NUL-terminated string atomically --------
        // x2 = pointer to a NUL-terminated string. Measures its length and writes
        // the whole thing with a single `DebugWrite` (syscall 19), so the line is
        // held together under one hold of the kernel console lock instead of being
        // shredded byte-by-byte into another core's output the way the old
        // per-byte `DebugPutc` loops were. Reached only via `bl`; every caller does
        // an `Exit` or reloads what it needs, so falling through here never happens.
        // Clobbers x0, x1, x8, x9 and the link register.
        ".Lputs:",
        "mov x9, x2",                // x9 = scan cursor
        ".Lputs_len:",
        "ldrb w0, [x9], #1",
        "cbnz w0, .Lputs_len",
        "sub x1, x9, x2",            // length including the NUL
        "sub x1, x1, #1",            // exclude the NUL
        "mov x0, x2",                // ptr
        "mov x8, #19",               // Syscall::DebugWrite
        "svc #0",
        "ret",

        "8:",
        ".asciz \"server drove the UART, then revoked it for everyone\\n\"",
        "1:",
        ".asciz \"[driver] user-space UART-RX driver waiting for input\\n\"",
        "2:",
        ".asciz \"\\n[driver] newline received; user-space IRQ driver exiting\\n\"",
        "3:",
        ".asciz \"[memtest] 3 MiB .bss reaches 2.5 MiB in; grew heap by 4096 pages (16 MiB), all written and read back\\n\"",
        "4:",
        ".asciz \"[parent] spawned 6 children via the Spawn syscall - 13 tasks total, old table held 8\\n\"",
        "5:",
        ".asciz \"[child] hello - I was created at runtime, not by the kernel\\n\"",
        "6:",
        ".asciz \"[client] kernel refused a syscall pointer into an unmapped page - it walks our tables, not a range\\n\"",
        "9:",
        ".asciz \"shared-memory works: written by the server, read by the client\"",
        "10:",
        ".asciz \"[client] read from shared memory: \"",
        "11:",
        ".asciz \"[memtest] DMA buffer: 4 physically-contiguous non-cacheable pages, first and last written and read back\\n\"",
        marker = sym DATA_MARKER,
        scratch = sym BSS_SCRATCH,
        big = sym BIG_BSS,
    )
}

/// Three megabytes of `.bss`, which exists to make this image *large*.
///
/// It costs nothing in the file (`memsz > filesz`; the loader zero-fills the
/// tail) but it makes the writable segment span more than one 2 MiB level-2
/// entry, so mapping it requires the kernel to build several leaf tables. Both
/// facts used to be fatal: an EL0 window of one leaf table could not hold it, and
/// an address space's fixed 32-frame ownership list could not track its 768
/// frames. The memtest task reads and writes the far end to prove it is really
/// there — and every task's teardown has to give all of it back.
#[no_mangle]
static mut BIG_BSS: [u8; 3 * 1024 * 1024] = [0; 3 * 1024 * 1024];

/// An initialized byte in the writable data segment (`.data`). The server reads
/// it to prove the ELF loader copied the R-W `PT_LOAD` segment's file image into
/// place. Printed to the console (`'#'`) ahead of the greeting.
#[no_mangle]
static mut DATA_MARKER: u8 = b'#';

/// A byte in the zero-initialized segment (`.bss`). The server writes the marker
/// here and reads it back, proving the R-W segment is genuinely writable and that
/// the loader zero-filled the `memsz > filesz` tail.
#[no_mangle]
static mut BSS_SCRATCH: u8 = 0;

/// With `panic = "abort"` and a naked entry that never panics, this is only here
/// to satisfy `no_std` linkage. It is unreachable at runtime.
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}
