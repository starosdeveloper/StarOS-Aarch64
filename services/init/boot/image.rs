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
/// Send=1, Recv=2, MapMemory=3, Exit=4, DebugPutc=5, Revoke=6, DebugWrite=19,
/// ClockNow=20, SleepUntil=21, NotifyCreate=22, NotifySignal=23, WaitAny=24,
/// SpawnThread=25.
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
        "sub sp, sp, #16",           // and a small array for WaitAny handles
        "mov x14, sp",               // x14 = &handles[0]
        "cmp w19, #1",
        "b.ne 4f",                   // id != 1 -> server

        // ================= client (id 1) =================
        // First, the monotonic clock. This task holds no capabilities at all yet,
        // which is the point: reading the time is not a privilege. Two `ClockNow`
        // reads with a spin between them must come back non-zero and strictly
        // increasing — the three failures that matter are a clock that never
        // started (zero), one the kernel refused (a negative error, which compares
        // as a huge unsigned and so also fails the "strictly later" test), and one
        // that stands still.
        "mov x8, #20",               // Syscall::ClockNow
        "svc #0",
        "mov x26, x0",               // x26 = first reading
        "movz x28, #0x4000",         // a short spin, so the readings straddle work
        ".Lcl_spin:",
        "subs x28, x28, #1",
        "b.ne .Lcl_spin",
        "mov x8, #20",               // Syscall::ClockNow again
        "svc #0",
        "mov x27, x0",               // x27 = second reading
        "cbz x26, .Lcl_clock_bad",   // zero: the clock never started
        "cmp x27, x26",
        "b.ls .Lcl_clock_bad",       // not strictly later: stopped, or an error
        "adr x2, 16f",
        "bl .Lputs",
        "b .Lcl_clock_done",
        ".Lcl_clock_bad:",
        "adr x2, 17f",
        "bl .Lputs",
        ".Lcl_clock_done:",

        // Sleep against an ABSOLUTE deadline 20 ms out, and check the result from
        // both sides. Waking early would mean the deadline was not honoured;
        // waking after a whole 100 ms tick period would mean the kernel never
        // shortened its timer and simply rounded the sleep up to the next tick —
        // which is the difference between a usable frame deadline and a useless
        // one. Both bounds are checked here because only one of them fails
        // visibly.
        "mov x8, #20",               // Syscall::ClockNow
        "svc #0",
        "mov x26, x0",               // t0
        "movz x24, #0x2D00",         // 20_000_000 ns
        "movk x24, #0x0131, lsl #16",
        "add x0, x26, x24",          // absolute deadline = now + 20 ms
        "mov x8, #21",               // Syscall::SleepUntil
        "svc #0",
        "mov x8, #20",               // Syscall::ClockNow
        "svc #0",
        "sub x25, x0, x26",          // elapsed
        "cmp x25, x24",
        "b.lo .Lcl_sleep_bad",       // woke BEFORE the deadline
        // Only the lower bound is checked here, and deliberately so. An upper bound
        // measured from EL0 times the whole round trip — park, wake, be *scheduled*,
        // read the clock — so on a loaded four-core run it fails for reasons that
        // have nothing to do with the timer. Whether the sleep itself was tight is
        // the kernel's `worst overshoot` line, which measures the wake-up alone.
        // A deadline already in the past must return at once rather than park. The
        // kernel counts these separately, and the count is what proves the path was
        // taken — from here it is indistinguishable from a very short sleep.
        "mov x0, #1",                // 1 ns after boot: long gone
        "mov x8, #21",               // Syscall::SleepUntil
        "svc #0",
        "adr x2, 20f",
        "bl .Lputs",
        "b .Lcl_sleep_done",
        ".Lcl_sleep_bad:",
        "adr x2, 21f",
        "bl .Lputs",
        ".Lcl_sleep_done:",


        // The client holds *no* device authority. It asks the server for the
        // UART by sending a request, then tries to use the capability the server
        // delegates back — and discovers the server has already revoked it.
        //
        // Two notifications of our own. The first is a source nothing will ever
        // signal — it is there to prove `WaitAny` reports *which* one fired, not
        // merely that something did. The second is delegated to the server below,
        // so the wake-up comes from another task entirely.
        "mov x8, #22",               // Syscall::NotifyCreate
        "svc #0",
        "mov x12, x0",               // x12 = silent notification handle
        "mov x8, #22",               // Syscall::NotifyCreate
        "svc #0",
        "mov x13, x0",               // x13 = notification the server will signal
        "str w12, [x14]",            // handles[0] = the silent one
        "str w13, [x14, #4]",        // handles[1] = the server's

        // Build a request message: tag = 1, no payload, carrying the notification
        // capability the server should signal.
        "stp xzr, xzr, [x11]",
        "stp xzr, xzr, [x11, #16]",
        "stp xzr, xzr, [x11, #32]",
        "mov x0, #1",
        "str x0, [x11]",             // msg.tag = 1
        "str w13, [x11, #40]",       // msg.cap = our notification, delegated
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
        // The server left its marker on the second page of a two-page buffer, so
        // that is where we read it from — proving both pages of one shared object
        // arrived in this space, at our own address.
        "add x23, x0, #4096",        // x23 = second page of our mapping
        "adr x2, 10f",               // "[client] read from shared memory: "
        "bl .Lputs",
        "mov x2, x23",               // the NUL-terminated bytes the server left there
        "bl .Lputs",
        "mov w0, #0x0a",             // trailing newline (a lone byte: DebugPutc is fine)
        "mov x8, #5",                // Syscall::DebugPutc
        "svc #0",
        "adr x2, 19f",               // and say which page those bytes came from
        "bl .Lputs",

        // Wait on BOTH notifications with a two-second deadline. Only the second
        // is ever signalled, and it is signalled by the *server* — so this both
        // parks against a cross-task wake-up and checks that the index reported is
        // the one that actually fired rather than the first in the list.
        "mov x8, #20",               // Syscall::ClockNow
        "svc #0",
        "movz x24, #0x9400",         // 2_000_000_000 ns
        "movk x24, #0x7735, lsl #16",
        "add x2, x0, x24",           // deadline = now + 2 s
        "mov x0, x14",               // &handles[0]
        "mov x1, #2",                // two of them
        "mov x8, #24",               // Syscall::WaitAny
        "svc #0",
        "cmp x0, #1",
        "b.ne .Lcl_wait_bad",        // must be index 1, the server's notification

        // And a wait that must time out — on BOTH handles again, 20 ms. Two things
        // at once: the deadline is honoured when nothing fires, and the signal the
        // wait above returned was really *consumed*. A kernel that reported
        // readiness without decrementing the count would answer this one instantly
        // with index 1 instead of timing out.
        "mov x8, #20",               // Syscall::ClockNow
        "svc #0",
        "movz x24, #0x2D00",         // 20_000_000 ns
        "movk x24, #0x0131, lsl #16",
        "add x2, x0, x24",
        "mov x0, x14",
        "mov x1, #2",
        "mov x8, #24",               // Syscall::WaitAny
        "svc #0",
        "cmp x0, #0",
        "b.ge .Lcl_wait_bad",        // returned an index?! nothing signalled it

        // The wait above parked and timed out — which means we were registered as
        // that notification's waiter and then left. Signal it now: nobody is
        // waiting, so the signal must be *counted* and handed to the next wait. If
        // the timed-out wait failed to deregister, the notification still thinks we
        // are waiting, hands this signal to a task that is not listening, and it
        // vanishes — leaving the wait below to time out instead of returning 0.
        "mov x0, x12",
        "mov x8, #23",               // Syscall::NotifySignal (our own, silent one)
        "svc #0",
        "mov x8, #20",               // Syscall::ClockNow
        "svc #0",
        "movz x24, #0x2D00",         // 20_000_000 ns
        "movk x24, #0x0131, lsl #16",
        "add x2, x0, x24",
        "mov x0, x14",
        "mov x1, #1",
        "mov x8, #24",               // Syscall::WaitAny
        "svc #0",
        "cmp x0, #0",
        "b.ne .Lcl_wait_bad",        // must be index 0, from the pending signal

        // Last: a stale registration must not poison the *next* block. Signal the
        // notification once more with nobody waiting — if the waits above left us
        // registered, the kernel hands this wake to a task that is not listening,
        // which arms its "a wakeup arrived before you parked" flag. The sleep below
        // would then return instantly instead of sleeping, and the damage would
        // land somewhere with no obvious connection to notifications at all.
        "mov x0, x12",
        "mov x8, #23",               // Syscall::NotifySignal
        "svc #0",
        "mov x8, #20",               // Syscall::ClockNow
        "svc #0",
        "mov x26, x0",
        "movz x24, #0x2D00",         // 20_000_000 ns
        "movk x24, #0x0131, lsl #16",
        "add x0, x26, x24",
        "mov x8, #21",               // Syscall::SleepUntil
        "svc #0",
        "mov x8, #20",               // Syscall::ClockNow
        "svc #0",
        "sub x25, x0, x26",
        "cmp x25, x24",
        "b.lo .Lcl_wait_bad",        // did not sleep: a stale wakeup was consumed
        "adr x2, 22f",
        "bl .Lputs",
        "b .Lcl_wait_done",
        ".Lcl_wait_bad:",
        "adr x2, 23f",
        "bl .Lputs",
        ".Lcl_wait_done:",

        // ---- a thread in this very address space ----
        // One page of our own heap is all the two of us need to talk: threads share
        // every page, so an address means the same thing on both sides. That is the
        // whole difference from `Spawn`, where the child gets its own space and this
        // pointer would name a different frame.
        "mov x0, #1",
        "mov x8, #10",               // Syscall::MapAnon
        "svc #0",
        "mov x15, x0",               // x15 = the shared page
        "str xzr, [x15]",            // where the thread will leave its marker
        "str xzr, [x15, #16]",       // and the thread pointer it saw
        // A notification of its own. The earlier one still carries the signal the
        // stale-registration check left pending, and waiting on that would return
        // instantly — the thread would look finished before it had started.
        "mov x8, #22",               // Syscall::NotifyCreate
        "svc #0",
        "mov x13, x0",               // x13 = the thread's notification
        "str w13, [x14]",            // handles[0] = it
        "str w13, [x15, #8]",        // and tell the thread which one to signal
        "adr x0, .Lthread",          // EL0 entry for the thread
        "mov x1, #4",                // four pages of stack, its own
        "movz x2, #0x5A5A",          // its thread pointer (TPIDR_EL0)
        "mov x3, x15",               // argument: the shared page
        "mov x8, #25",               // Syscall::SpawnThread
        "svc #0",
        "cmp x0, #0",
        "b.lt .Lcl_thread_bad",      // could not create it
        // Wait for it to finish, with a deadline so a thread that never runs is a
        // failure rather than a hang.
        "mov x8, #20",               // Syscall::ClockNow
        "svc #0",
        "movz x24, #0x9400",         // 2_000_000_000 ns
        "movk x24, #0x7735, lsl #16",
        "add x2, x0, x24",
        "mov x0, x14",               // handles[0] = the silent notification
        "mov x1, #1",
        "mov x8, #24",               // Syscall::WaitAny
        "svc #0",
        "cmp x0, #0",
        "b.ne .Lcl_thread_bad",      // timed out: the thread never signalled
        // It wrote through *our* page, at the address we gave it.
        "ldr x20, [x15]",
        "movz x24, #0xF00D",
        "cmp x20, x24",
        "b.ne .Lcl_thread_bad",
        // And it ran with its own thread pointer, not ours. Ours was never set, so
        // the two must differ — that difference is what makes `thread_local` work,
        // and it only holds if the register rides in the saved context.
        "ldr x20, [x15, #16]",       // TPIDR_EL0 as the thread saw it
        "movz x24, #0x5A5A",
        "cmp x20, x24",
        "b.ne .Lcl_thread_bad",
        "mrs x21, tpidr_el0",        // and as WE see it, back on this thread
        "cmp x21, x20",
        "b.eq .Lcl_thread_bad",      // same value: the register is not per-thread
        // The thread has exited. Our address space must have survived it — the last
        // one out tears it down, not the first. Proving that needs the freed frames
        // to be *reused*: claim 64 fresh pages, write through them, then read our
        // original marker back. If the thread's exit had destroyed the space, its
        // page tables would now be in the allocator's pool, handed straight back out
        // here, and this either faults or reads something that is no longer ours.
        "mov x0, #64",
        "mov x8, #10",               // Syscall::MapAnon
        "svc #0",
        "cmp x0, #0",
        "b.lt .Lcl_thread_bad",
        "movz x24, #0xBEEF",
        "str x24, [x0]",             // touch the new memory (needs live tables)
        "ldr x20, [x15]",            // and our marker must still be there
        "movz x24, #0xF00D",
        "cmp x20, x24",
        "b.ne .Lcl_thread_bad",
        "adr x2, 24f",
        "bl .Lputs",
        "b .Lcl_thread_done",
        ".Lcl_thread_bad:",
        "adr x2, 25f",
        "bl .Lputs",
        ".Lcl_thread_done:",
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
        // Receive the client's request on ep0 (handle 1). It carries a
        // notification capability the client is waiting on — the kernel installs a
        // copy in our table and writes its handle into msg.cap.
        "mov x0, #1",
        "mov x1, x11",
        "mov x8, #2",                // Syscall::Recv
        "svc #0",
        "ldr w27, [x11, #40]",       // w27 = the client's notification, delegated
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
        "mov x0, #2",                // two pages, not one
        "mov x8, #14",               // Syscall::CreateShared
        "svc #0",
        "mov x25, x0",               // x25 = shared capability handle
        "mov x0, x25",
        "mov x8, #15",               // Syscall::MapShared
        "svc #0",
        // Write the marker into the SECOND page. Both halves of the buffer have to
        // survive the round trip: the object is described by one (phys, pages)
        // pair, and a mapping that honoured only the first page would leave this
        // store faulting here and the client reading nothing.
        "add x26, x0, #4096",        // x26 = second page of the shared buffer
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
        // Wake the client. It is parked in `WaitAny` on two notifications, and
        // this is the one it was given — so the wake-up crosses tasks, and the
        // index it reports has to be ours rather than the silent one.
        "mov x0, x27",
        "mov x8, #23",               // Syscall::NotifySignal
        "svc #0",
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
        "movz x4, #0x24, lsl #16",   // + 2.25 MiB (past the 2 MiB L2 boundary)
        "add x3, x3, x4",
        "ldr x5, [x3]",              // the loader must have zero-filled this tail
        "cbnz x5, .Lmt_exit",        // not zero -> exit quietly (no report)
        "str x1, [x3]",
        "ldr x2, [x3]",
        "cmp x1, x2",
        "b.ne .Lmt_exit",            // mismatch -> exit quietly (no report)
        // (b) Grow the heap by far more pages than a fixed frame list ever held.
        // Sixteen mebibytes, in EIGHT calls of 1024 pages rather than the 4096
        // single-page calls this used to take — the point of the multi-page
        // `MapAnon`. Each call may make the kernel build new tables.
        //
        // Every run is checked at BOTH ends: a marker into the first page and into
        // the last page of the same run, both read back. A kernel that honoured
        // the count only for the first page would pass a check of the first page
        // and fault (or read rubbish) at the last.
        "mov w23, #8",               // runs to make
        "movz x26, #0x400",          // pages per run (1024 = 4 MiB)
        "mov x27, xzr",              // VA the previous run ended at
        ".Lmt_grow:",
        "mov x0, x26",               // pages
        "mov x8, #10",               // Syscall::MapAnon
        "svc #0",
        "cmp x0, #0",
        "b.lt .Lmt_exit",            // pool exhausted -> exit quietly (no report)
        "str x1, [x0]",              // marker into the FIRST page of the run
        "ldr x2, [x0]",
        "cmp x1, x2",
        "b.ne .Lmt_exit",
        "lsl x3, x26, #12",          // run length in bytes
        "add x4, x0, x3",
        "sub x4, x4, #8",            // last word of the LAST page of the run
        // Fresh anonymous memory is zero-filled, and that has to hold for the far
        // end of a run as much as for its first page — a page recycled from a dead
        // task must not arrive carrying what it last held.
        "ldr x2, [x4]",              // faults here if the count was ignored
        "cbnz x2, .Lmt_exit",        // not zeroed -> exit quietly (no report)
        "str x1, [x4]",
        "ldr x2, [x4]",
        "cmp x1, x2",
        "b.ne .Lmt_exit",
        // Runs must be handed out back to back, with no gap and no overlap: the
        // heap cursor has to move by exactly what was mapped. Skip the check on
        // the first run, which has no predecessor.
        "cbz x27, .Lmt_first",
        "cmp x0, x27",
        "b.ne .Lmt_exit",            // gap or overlap -> exit quietly (no report)
        ".Lmt_first:",
        "add x27, x0, x3",           // where this run ends = where the next starts
        "subs w23, w23, #1",
        "b.ne .Lmt_grow",
        // A zero-page request must be refused, not quietly rounded up to one. This
        // is the one case where a wrong answer is invisible in normal use.
        "mov x0, xzr",
        "mov x8, #10",               // Syscall::MapAnon(0)
        "svc #0",
        "cmp x0, #0",
        "b.ge .Lmt_exit",            // accepted?! say nothing and leave
        "adr x2, 18f",
        "bl .Lputs",
        // `.Lputs` clobbers x0/x1/x8/x9, and the DMA half below still needs the
        // magic value. Rebuild it rather than reserving another register.
        "movz x1, #0xBEEF",
        "movk x1, #0xDEAD, lsl #16",
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
        // Sleep 100 ms before the final report. This task finishes last, so for
        // that stretch the only thing left in the system is asleep — which is the
        // one arrangement that can tell a scheduler counting `Sleeping` as work
        // from one that does not. A scheduler that treats sleep as "nothing left
        // to do" ends the run here and the line below is never printed.
        "mov x8, #20",               // Syscall::ClockNow
        "svc #0",
        "movz x24, #0xE100",         // 100_000_000 ns
        "movk x24, #0x05F5, lsl #16",
        "add x0, x0, x24",
        "mov x8, #21",               // Syscall::SleepUntil
        "svc #0",
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
        "b.ne .Lstorm_send",         // id != 6 -> the IPC storm roles, then the child
        // Three children. The kernel already starts twelve tasks of its own, so the
        // total lands at fifteen — well past the old `MAX_TASKS = 8`, where `Spawn`
        // returned failure. Each child announces itself, so the kernel's own
        // `sched::task_count` (printed at the end) is the proof.
        //
        // Three rather than six because every spawned child is another 3 MiB
        // address space, and the 128 MiB machine in the smoke matrix has to hold
        // all of them at once. The count proves the table grew; it does not need to
        // prove it twice.
        "mov w20, #3",               // children to create
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

        // ============ IPC storm sender (id 9) ============
        // Three of these run at once, each sending STORM_MSGS messages into ONE
        // endpoint whose ring holds two. The point is contention: with three
        // senders and a two-slot ring, most sends block and wake, on whichever
        // cores the scheduler happens to be using. Each message carries its
        // sequence number in words[0], so the receiver can check the *sum* — a lost
        // or duplicated message cannot balance.
        ".Lstorm_send:",
        "cmp w19, #9",
        "b.ne .Lstorm_recv",         // id != 9 -> the receiver
        "mov w20, #{storm_msgs}",    // messages left to send
        "mov x21, #1",               // sequence number, 1..=STORM_MSGS
        ".Lst_loop:",
        "stp xzr, xzr, [x11]",
        "stp xzr, xzr, [x11, #16]",
        "stp xzr, xzr, [x11, #32]",
        "mov x0, #9",
        "str x0, [x11]",             // msg.tag = 9 (storm)
        "str x21, [x11, #8]",        // msg.words[0] = sequence number
        "mov x0, #1",                // handle 1 = the storm endpoint (send)
        "mov x1, x11",
        "mov x8, #1",                // Syscall::Send (blocks whenever the ring is full)
        "svc #0",
        "add x21, x21, #1",
        "subs w20, w20, #1",
        "b.ne .Lst_loop",
        "mov x8, #4",                // Syscall::Exit
        "svc #0",

        // ============ IPC storm receiver (id 10) ============
        // Drains exactly STORM_SENDERS * STORM_MSGS messages and checks the total.
        // The expected sum is fixed at build time (see STORM_SUM): senders each
        // count 1..=STORM_MSGS, so the sum is senders * msgs * (msgs+1) / 2.
        // Reporting only "OK" or "MISMATCH" keeps this printable from naked
        // assembly, and either line is decisive.
        ".Lstorm_recv:",
        "cmp w19, #10",
        "b.ne .Lstackgrow",          // id != 10 -> the stack grower, then the child
        "mov x22, xzr",              // messages received
        "mov x23, xzr",              // running sum of sequence numbers
        "mov w24, #{storm_total}",   // how many to expect
        ".Lsr_loop:",
        "mov x0, #1",                // handle 1 = the storm endpoint (recv)
        "mov x1, x11",
        "mov x8, #2",                // Syscall::Recv (blocks until a sender arrives)
        "svc #0",
        "ldr x0, [x11, #8]",         // msg.words[0] = sequence number
        "add x23, x23, x0",
        "add x22, x22, #1",
        "cmp w22, w24",
        "b.lo .Lsr_loop",
        // Every message arrived. Does the arithmetic agree?
        "movz x1, #{storm_sum_lo}",
        "movk x1, #{storm_sum_hi}, lsl #16",
        "cmp x23, x1",
        "b.ne .Lsr_bad",
        "adr x2, 12f",               // storm OK string
        "bl .Lputs",
        "mov x8, #4",                // Syscall::Exit
        "svc #0",
        ".Lsr_bad:",
        "adr x2, 13f",               // storm MISMATCH string
        "bl .Lputs",
        "mov x8, #4",                // Syscall::Exit
        "svc #0",

        // ============ stack grower (id 11) ============
        // Walks the stack pointer down one page at a time, writing a marker into
        // each page as it goes. Only ONE stack page is mapped when a task starts, so
        // every step past the first takes a translation fault that the kernel turns
        // into a mapped page and retries — this task exists to make that happen and
        // then prove the pages are real by walking back up and reading the markers
        // (a fault handler that mapped the wrong page, or the same page twice, fails
        // the read-back rather than passing quietly).
        //
        // Then it deliberately runs past the limit: the final store lands in the
        // guard region, which the kernel refuses to grow into, and the task is
        // killed. That kill is the point — a stack that grows without bound would
        // eat the frame pool instead.
        ".Lstackgrow:",
        "cmp w19, #11",
        "b.ne .Lfbclient",           // id != 11 -> the display client, then the rest
        "mov x25, sp",               // remember the top so we can restore it
        "mov w20, #{stack_pages}",   // pages to walk down
        "mov x21, sp",
        "mov x22, #0x1000",
        ".Lsg_down:",
        "sub x21, x21, x22",         // one page lower
        "mov sp, x21",               // move the real stack pointer with it
        "str x20, [x21]",            // fault -> kernel maps this page -> retry lands here
        "subs w20, w20, #1",
        "b.ne .Lsg_down",
        // Walk back up and check every marker survived.
        "mov w20, #1",
        ".Lsg_up:",
        "ldr x0, [x21]",
        "cmp x0, x20",
        "b.ne .Lsg_bad",
        "add x21, x21, x22",
        "add w20, w20, #1",
        "cmp w20, #{stack_pages}",
        "b.le .Lsg_up",
        "mov sp, x25",               // back on the original stack
        "adr x2, 14f",               // stack-growth OK string
        "bl .Lputs",
        // Now overrun the limit on purpose: one store far below where growth stops.
        // The kernel must refuse and kill us; nothing after this line runs.
        "movz x0, #{stack_overrun_hi}, lsl #16",
        "sub x21, x25, x0",
        "str x20, [x21]",            // guard region -> fault -> task killed
        "mov x8, #4",                // Syscall::Exit (not reached)
        "svc #0",
        ".Lsg_bad:",
        "mov sp, x25",
        "adr x2, 15f",               // stack-growth MISMATCH string
        "bl .Lputs",
        "mov x8, #4",                // Syscall::Exit
        "svc #0",

        // ============ display client (id 13) ============
        // Holds two endpoint capabilities and nothing else — no device, no screen,
        // no authority. It allocates its own pixels, fills them, and asks the
        // display server to put them on the glass. The only thing it can reach is
        // memory it made itself.
        ".Lfbclient:",
        "cmp w19, #13",
        "b.ne .Linputclient",        // id != 13 -> the input consumer, then the rest
        // Two 64x64 surfaces, deliberately overlapping. One would prove a rectangle
        // reaches the glass; two prove the server keeps them apart, stacks them, and
        // repaints only what a commit says changed. Each is 16 KiB, four pages.
        //
        // The second buffer landing at its own address is itself a claim: until the
        // kernel kept one placement per shared object, every MapShared returned the
        // same address and the second surface simply replaced the first.
        "mov x0, #4",
        "mov x8, #14",               // Syscall::CreateShared
        "svc #0",
        "cmp x0, #0",
        "b.lt .Lfb_bad",
        "mov x21, x0",               // x21 = the red surface's capability handle
        "mov x8, #15",               // Syscall::MapShared
        "svc #0",
        "cmp x0, #0",
        "b.lt .Lfb_bad",
        "mov x22, x0",               // x22 = the red pixels, in our own space
        "movz w23, #0x00FF, lsl #16",  // 0x00FF0000, solid red
        "bl .Lfb_fill",
        "mov x0, #4",
        "mov x8, #14",               // Syscall::CreateShared
        "svc #0",
        "cmp x0, #0",
        "b.lt .Lfb_bad",
        "mov x26, x0",               // x26 = the blue surface's capability handle
        "mov x8, #15",               // Syscall::MapShared
        "svc #0",
        "cmp x0, #0",
        "b.lt .Lfb_bad",
        "mov x27, x0",               // x27 = the blue pixels
        "cmp x27, x22",
        "b.eq .Lfb_bad",             // two buffers, one address: the placement broke
        "mov x22, x27",
        "mov w23, #0x00FF",          // 0x000000FF, solid blue
        "bl .Lfb_fill",
        // Create the red surface at (100, 80).
        "mov x24, #100",
        "mov x25, #80",
        "mov w28, w21",
        "bl .Lfb_create",
        "mov x20, x0",               // x20 = the red surface's id
        // Create the blue one at (140, 110): its top-left quarter lies over the red
        // surface's bottom-right, which is the overlap the screen check reads.
        "mov x24, #140",
        "mov x25, #110",
        "mov w28, w26",
        "bl .Lfb_create",
        "mov x21, x0",               // x21 = the blue surface's id (its cap is done)
        // Commit both in full. 64*64 = 4096 pixels each.
        "mov x0, x20",
        "mov x24, #64",
        "mov x25, #64",
        "bl .Lfb_commit",
        "mov x26, #4096",
        "cmp x0, x26",
        "b.ne .Lfb_bad",
        "mov x0, x21",
        "mov x24, #64",
        "mov x25, #64",
        "bl .Lfb_commit",
        "cmp x0, x26",
        "b.ne .Lfb_bad",
        // Raise the red surface. It was created first, so it was underneath; after
        // this the overlap must be red, and that is what the screenshot checks.
        "stp xzr, xzr, [x11]",
        "stp xzr, xzr, [x11, #16]",
        "stp xzr, xzr, [x11, #32]",
        "mov x0, #4",
        "str x0, [x11]",             // msg.tag = 4 (Raise)
        "str x20, [x11, #8]",        // words[0] = the red surface
        "bl .Lfb_call",
        "cmp x0, x26",
        "b.ne .Lfb_bad",             // a raise repaints the whole surface: 4096
        // Commit an 8x8 corner of it. The reply must be 64 pixels and not 4096:
        // damage that is not honoured is a full-frame copy wearing a smaller number,
        // and at 1080p that is the difference between sixty frames and a slide show.
        "mov x0, x20",
        "mov x24, #8",
        "mov x25, #8",
        "bl .Lfb_commit",
        "mov x27, #64",
        "cmp x0, x27",
        "b.ne .Lfb_bad",
        // Say goodbye, so the server ends its loop rather than being killed inside
        // it. A server that only exits by dying never proves it can stop cleanly.
        "stp xzr, xzr, [x11]",
        "stp xzr, xzr, [x11, #16]",
        "stp xzr, xzr, [x11, #32]",
        "mov x0, #9",
        "str x0, [x11]",             // msg.tag = 9 (Bye)
        "bl .Lfb_call",
        "adr x2, 27f",
        "bl .Lputs",
        "mov x8, #4",                // Syscall::Exit
        "svc #0",
        ".Lfb_bad:",
        "adr x2, 28f",
        "bl .Lputs",
        "mov x8, #4",                // Syscall::Exit
        "svc #0",

        // -------- fbclient helpers --------
        // Fill 4096 pixels at x22 with the colour in w23.
        ".Lfb_fill:",
        "mov x9, #4096",
        "mov x10, x22",
        ".Lfb_fill_loop:",
        "str w23, [x10], #4",
        "subs x9, x9, #1",
        "b.ne .Lfb_fill_loop",
        "ret",
        // Create a 64x64 surface at (x24, y25) from the capability in w28; returns
        // its id in x0.
        ".Lfb_create:",
        "mov x12, x30",              // .Lputs and the call below clobber x30
        "stp xzr, xzr, [x11]",
        "stp xzr, xzr, [x11, #16]",
        "stp xzr, xzr, [x11, #32]",
        "mov x0, #1",
        "str x0, [x11]",             // msg.tag = 1 (Create)
        "mov x0, #64",
        "str x0, [x11, #8]",         // words[0] = width
        "str x0, [x11, #16]",        // words[1] = height
        "str x24, [x11, #24]",       // words[2] = x
        "str x25, [x11, #32]",       // words[3] = y
        "str w28, [x11, #40]",       // msg.cap = the buffer, delegated
        "bl .Lfb_call",
        "mov x30, x12",
        "ret",
        // Commit the surface in x0 with a damage rectangle of (x24, x25) at its
        // origin; returns the pixels the server wrote.
        ".Lfb_commit:",
        "mov x12, x30",
        "mov x13, x0",
        "stp xzr, xzr, [x11]",
        "stp xzr, xzr, [x11, #16]",
        "stp xzr, xzr, [x11, #32]",
        "mov x0, #3",
        "str x0, [x11]",             // msg.tag = 3 (Commit)
        "str x13, [x11, #8]",        // words[0] = surface id
        // words[1] = damage origin, x | y << 32, both zero here.
        "lsl x0, x25, #32",
        "orr x0, x0, x24",
        "str x0, [x11, #24]",        // words[2] = w | h << 32
        "bl .Lfb_call",
        "mov x30, x12",
        "ret",
        // Send the message at x11 and wait for the reply; returns words[0] in x0,
        // or jumps to the failure path if the server refused.
        ".Lfb_call:",
        "mov x14, x30",
        "mov x0, #1",                // handle 1 = the display endpoint (send)
        "mov x1, x11",
        "mov x8, #1",                // Syscall::Send
        "svc #0",
        "mov x0, #2",                // handle 2 = the reply endpoint (recv)
        "mov x1, x11",
        "mov x8, #2",                // Syscall::Recv
        "svc #0",
        "ldr x0, [x11]",             // reply tag: 2 is ok, 0 is a refusal
        "cmp x0, #2",
        "b.ne .Lfb_bad",
        "ldr x0, [x11, #8]",         // words[0] = the result
        "mov x30, x14",
        "ret",

        // ============ input consumer (id 16) ============
        // Holds one capability: receive on the event endpoint. No device, no
        // interrupt, no sight of the driver's memory — a key press reaches this
        // process as a message or not at all. It blocks until one arrives, prints
        // the code it was told, and exits.
        //
        // On a machine where nobody presses a key it simply never returns, and that
        // is the honest behaviour: a consumer that gave up after a while would
        // print "no input" on a working system whose user was slow.
        ".Linputclient:",
        "cmp w19, #16",
        "b.ne .Lloaded",             // id != 16 -> the loaded-from-a-file role
        "mov x0, #1",                // handle 1 = the event endpoint (recv)
        "mov x1, x11",
        "mov x8, #2",                // Syscall::Recv
        "svc #0",
        "cmp x0, #0",
        "b.lt .Lic_bad",
        "ldr x20, [x11]",            // msg.tag = the event kind
        "cmp x20, #1",               // 1 = EV_KEY, the only kind published so far
        "b.ne .Lic_bad",
        "adr x2, 29f",
        "bl .Lputs",
        "ldr x0, [x11, #8]",         // words[0] = the key code
        "bl .Lputdec",
        "adr x2, 30f",
        "bl .Lputs",
        "mov x8, #4",                // Syscall::Exit
        "svc #0",
        ".Lic_bad:",
        "adr x2, 31f",
        "bl .Lputs",
        "mov x8, #4",                // Syscall::Exit
        "svc #0",

        // ============ loaded from a file (id 12) ============
        // Same bytes as every other role here, but this copy did not come from the
        // kernel's built-in image: the device manager found `init.elf` inside the
        // CPIO archive and handed those bytes to `SpawnImage`. Only this path seeds
        // id 12, so the line below can have arrived no other way.
        ".Lloaded:",
        "cmp w19, #12",
        "b.ne .Lchild",              // id != 12 -> a spawned child (id 8)
        "adr x2, 26f",
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

        // -------- the thread body (entered from SpawnThread) --------
        // Reached at EL0 with x0 = the argument its creator passed, on a stack the
        // kernel mapped for it alone, with its own TPIDR_EL0. It shares everything
        // else — including the page x0 points at, which is how it reports back.
        ".Lthread:",
        "mov x19, x0",               // x19 = the shared page
        "mrs x1, tpidr_el0",         // the thread pointer we were given
        // Use the stack before trusting it. A thread handed the *bottom* of its
        // stack region instead of the top faults on this first push, into the page
        // below — and without a push nothing here would ever touch the stack, so
        // the mistake would pass unnoticed.
        "str x1, [sp, #-16]!",
        "ldr x3, [sp], #16",
        "cmp x1, x3",
        "b.ne .Lthread_exit",        // stack did not hold: report nothing
        "str x1, [x19, #16]",
        "movz x2, #0xF00D",
        "str x2, [x19]",             // the marker, written through the shared page
        "ldr w0, [x19, #8]",         // the notification handle left for us
        "mov x8, #23",               // Syscall::NotifySignal — tell the creator
        "svc #0",
        ".Lthread_exit:",
        "mov x8, #4",                // Syscall::Exit — this thread ends, not the process
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

        // Print the unsigned value in x0 in decimal. Digits come out least
        // significant first, so they are written backwards into a scratch buffer
        // below the stack pointer and the whole run is sent in one `DebugWrite` —
        // one call is one line as far as the console lock is concerned, and a
        // number shredded across two cores' output is a number nobody can read.
        // Clobbers x0..x6, x8 and the link register.
        ".Lputdec:",
        "sub sp, sp, #32",
        "add x3, sp, #32",           // one past the end of the scratch buffer
        "mov x4, #10",
        ".Lputdec_digit:",
        "udiv x5, x0, x4",
        "msub x6, x5, x4, x0",       // x6 = x0 - (x0 / 10) * 10
        "add w6, w6, #48",           // '0'
        "strb w6, [x3, #-1]!",
        "mov x0, x5",
        "cbnz x0, .Lputdec_digit",
        "add x1, sp, #32",
        "sub x1, x1, x3",            // length
        "mov x0, x3",                // ptr
        "mov x8, #19",               // Syscall::DebugWrite
        "svc #0",
        "add sp, sp, #32",
        "ret",

        "8:",
        ".asciz \"server drove the UART, then revoked it for everyone\\n\"",
        "1:",
        ".asciz \"[driver] user-space UART-RX driver waiting for input\\n\"",
        "2:",
        ".asciz \"\\n[driver] newline received; user-space IRQ driver exiting\\n\"",
        "3:",
        ".asciz \"[memtest] 2.5 MiB .bss reaches 2.25 MiB in (past the 2 MiB L2 boundary); grew the heap by 16 MiB in 8 calls of 1024 pages, first and last page of every run zeroed then written and read back, runs handed out back to back\\n\"",
        "4:",
        ".asciz \"[parent] spawned 3 children via the Spawn syscall - 15 tasks total, old table held 8\\n\"",
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
        "12:",
        ".asciz \"[ipc-storm] receiver drained every message from 3 concurrent senders, sequence sum exact - no message lost or duplicated\\n\"",
        "13:",
        ".asciz \"[ipc-storm] SEQUENCE SUM MISMATCH - the endpoint lost or duplicated a message under contention\\n\"",
        "14:",
        ".asciz \"[stack] walked 40 pages down a stack that started with one mapped, every marker read back - pages arrived on demand\\n\"",
        "15:",
        ".asciz \"[stack] MARKER MISMATCH - a demand-mapped stack page was wrong\\n\"",
        "16:",
        ".asciz \"[client] monotonic clock: two ClockNow reads from EL0, the second strictly later - no capability needed\\n\"",
        "17:",
        ".asciz \"[client] CLOCK DID NOT ADVANCE - ClockNow returned zero, an error, or went backwards\\n\"",
        "18:",
        ".asciz \"[memtest] MapAnon(0) refused - a zero-page request is an error, not a page\\n\"",
        "19:",
        ".asciz \"[client] read the marker from the SECOND page of a 2-page shared buffer\\n\"",
        "20:",
        ".asciz \"[client] SleepUntil: woke no earlier than its 20 ms absolute deadline\\n\"",
        "21:",
        ".asciz \"[client] SLEEP WRONG - woke before the deadline it asked for\\n\"",
        "22:",
        ".asciz \"[client] WaitAny: index 1 of 2 from the server's notification, a lone silent source timed out, a later signal was still counted, and no stale registration poisoned the next block\\n\"",
        "23:",
        ".asciz \"[client] WAITANY WRONG - wrong index, or a wait with nothing to wake it returned anyway\\n\"",
        "24:",
        ".asciz \"[client] SpawnThread: a thread in this very address space wrote through our page and ran with its own TPIDR_EL0\\n\"",
        "25:",
        ".asciz \"[client] THREAD WRONG - it never ran, wrote nowhere we can see, or shared our thread pointer\\n\"",
        "26:",
        ".asciz \"[loaded] hello - my ELF was a file in the initramfs, parsed in user space and handed to the kernel as bytes\\n\"",
        "27:",
        ".asciz \"[fbclient] two 64x64 surfaces composited by displaysrv - overlapping, restacked, and an 8x8 commit repainted 64 pixels and not 4096\\n\"",
        "28:",
        ".asciz \"[fbclient] SURFACE WRONG - a buffer would not allocate, two of them landed at one address, the server refused a request, or it repainted a different rectangle than the commit named\\n\"",
        "29:",
        ".asciz \"[inputclient] key code \"",
        "30:",
        ".asciz \" arrived over IPC - I hold no device, no interrupt and no sight of the driver's memory\\n\"",
        "31:",
        ".asciz \"[inputclient] EVENT WRONG - the receive failed, or what arrived was not a key event\\n\"",
        marker = sym DATA_MARKER,
        scratch = sym BSS_SCRATCH,
        big = sym BIG_BSS,
        storm_msgs = const STORM_MSGS,
        storm_total = const (STORM_MSGS * STORM_SENDERS),
        storm_sum_lo = const (STORM_SUM & 0xffff),
        storm_sum_hi = const (STORM_SUM >> 16),
        stack_pages = const STACK_WALK_PAGES,
        stack_overrun_hi = const (STACK_OVERRUN_BYTES >> 16),
    )
}

/// How many pages the stack grower walks down. Comfortably inside the kernel's
/// `USER_STACK_MAX_PAGES` (64) so the walk itself always succeeds — the overrun
/// that follows is what tests the limit.
pub const STACK_WALK_PAGES: u32 = 40;

/// How far below the initial stack pointer the deliberate overrun reaches: past
/// the 64-page growth limit, into the guard region the kernel never fills.
///
/// A single `movz ..., lsl #16` builds it, so it must be a whole multiple of
/// 65536 — checked here rather than discovered as a wrong address at runtime.
pub const STACK_OVERRUN_BYTES: u32 = 80 * 4096;
const _: () = assert!(STACK_OVERRUN_BYTES.is_multiple_of(1 << 16));
const _: () = assert!((STACK_OVERRUN_BYTES >> 16) <= 0xffff);

/// How many messages each storm sender pushes into the shared endpoint.
///
/// Large enough that the two-slot ring blocks the senders repeatedly (that is the
/// contention being tested) and small enough that the run stays quick under TCG.
pub const STORM_MSGS: u32 = 64;

/// How many senders hammer the endpoint at once. Three fits the endpoint's
/// four-deep sender wait queue with room to spare, and needs more than one core to
/// overlap.
pub const STORM_SENDERS: u32 = 3;

/// The sum the receiver must see: each sender counts `1..=STORM_MSGS`, so the total
/// is `senders * msgs * (msgs + 1) / 2`. Checking the *sum* rather than the count
/// alone is what catches duplication: a message delivered twice and another lost
/// keeps the count right and moves the sum.
pub const STORM_SUM: u32 = STORM_SENDERS * STORM_MSGS * (STORM_MSGS + 1) / 2;

/// Two and a half megabytes of `.bss`, which exists to make this image *large*.
///
/// It costs nothing in the file (`memsz > filesz`; the loader zero-fills the
/// tail) but it makes the writable segment span more than one 2 MiB level-2
/// entry, so mapping it requires the kernel to build several leaf tables. Both
/// facts used to be fatal: an EL0 window of one leaf table could not hold it, and
/// an address space's fixed 32-frame ownership list could not track its 768
/// frames. The memtest task reads and writes the far end to prove it is really
/// there — and every task's teardown has to give all of it back.
/// Sized at 2.5 MiB rather than 3: every one of the dozen boot processes carries a
/// private copy, and the 128 MiB machine in the smoke matrix has only 32 MiB of
/// frames to hold them all. What the test needs is a writable segment that spans
/// more than one 2 MiB level-2 entry — 2.5 MiB does that with room to reach past
/// the boundary and back.
#[no_mangle]
static mut BIG_BSS: [u8; 5 * 512 * 1024] = [0; 5 * 512 * 1024];

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
