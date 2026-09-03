# STAR OS Kernel — v0.2 (rewrite)

A `no_std` **microkernel for aarch64**, built as a clean multi-crate Cargo
workspace. It replaces an earlier monolithic single-crate design that has since
been deleted; what that prototype taught is written down in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

The x86_64 port lives in [`../kernel-pc`](../kernel-pc) and shares the portable
crates in this tree by path — not by copy.

Everything below runs today: EL2→EL1 entry, Linux/arm64 boot protocol, device
tree instead of hard-coded addresses, TTBR1 split with 4 KiB page tables, GICv2
**and** GICv3, PSCI + SMP, a preemptive scheduler, EL0 isolation, capabilities
with revocation, synchronous IPC, shared memory, an ELF loader, drivers and
interrupt handling in **user space**, an SMMUv3 enforced against a real bus
master, an initramfs, a framebuffer console, a monotonic clock user space can read
and sleep against, waiting on a set of sources with a deadline, threads inside one
address space, threads that pin themselves to a core and can *prove* they stayed
there, processes loaded from a file rather than from the kernel image, a
display server in user space that owns the screen and routes input to the window it
belongs to, a virtio-input driver that decodes real key presses and pointer
positions from two devices without the kernel seeing one, a file server that hands
files to processes holding no archive, and a **C program** — compiled by clang
against this tree's own libc — that prints, computes `sin(10¹⁵)` correctly,
allocates, sleeps, reads files through `FILE*`, lists a directory that exists only
as a prefix in a flat archive, runs `pthread`s with real thread-local storage and
blocks in `poll` until another thread writes to an eventfd, without a syscall in
sight. And a **C++ program** on top of
that, with `std::vector`, `std::string`, `std::thread` and a static object whose
destructor runs at exit.

## Layout

### Portable crates (pure logic, host-tested)

| Crate | Role |
|-------|------|
| `crates/abi` | Syscall numbers, error codes, capability `Handle` — the kernel↔user contract — and the scheduler's pick policy: affinity masks, the round-robin scan and the class bands, as pure functions with host tests, because a scan that wraps one slot short shows up as "the pinned task ran somewhere else, sometimes" and a fairness escape that always serves the bottom band shows up as a boot that never finishes |
| `crates/hal` | Hardware-abstraction **traits** (console, timer, interrupt controller) |
| `crates/mm` | Address types, buddy frame allocator, kernel heap, memory regions |
| `crates/ipc` | Fixed-size IPC `Message` / `Endpoint` |
| `crates/fdt` | Flattened Device Tree parser — the root of all portability |
| `crates/cpio` | `newc` archive reader (Linux initramfs format) |
| `crates/framebuffer` | 8×8 font + text console over an arbitrary pixel format |
| `crates/videocore` | Raspberry Pi VideoCore property-mailbox **messages** (no MMIO) |
| `crates/virtio` | Split-virtqueue layout, MMIO register map, input event format, configuration-space offsets, and the axis arithmetic that turns a device position into a fraction (no MMIO) |
| `crates/iommu` | SMMUv3 descriptor/queue bit layouts (no MMIO) |
| `crates/staros-libc` | The C library EL0 programs link against, all 282 symbols a real Qt 6 build needs: `str*`/`mem*`/`printf`/`scanf` and the whole of libm written from scratch, `malloc` over `MapAnon`, time and the calendar over `ClockNow`, files and `FILE*` and directories over the file server, `pthread`s with real thread-local storage over `SpawnThread`, `poll`/`eventfd`/`pipe` over `WaitAny`, and the process layer — `getpid`, `uname`, `setjmp`/`longjmp`, `backtrace` |

The split is deliberate and repeated: every subsystem whose bugs hide in *layout*
gets a pure crate with exact-value tests, and only the doorbell-ringing half is
board `unsafe`.

### Board and privileged crates

| Crate | Role |
|-------|------|
| `crates/arch-aarch64` | Boot stub (EL2→EL1, `Image` header), MMU/TTBR1, address spaces, exception vectors, GICv2/v3, generic timer, PSCI/SMP, PL011, cache maintenance, user copies, `ramfb`, VideoCore mailbox, SMMUv3, minimal PCIe ECAM |
| `crates/drivers` | Driver lifecycle traits (`probe`/`init`) — drivers get no kernel internals |
| `crates/kernel` | The privileged binary: scheduler, syscalls, capabilities, objects, IPC, IRQ dispatch, ELF loader, heap, console, notifications, SMP, IOMMU policy |

### User space

| Component | Role |
|-----------|------|
| `services/init` | First EL0 process; `boot/image.rs` flattens the ELF to a bootable `Image` |
| `services/devicemgr` | Device manager: parses the DTB **in user space**, mints device/IRQ capabilities from an authority cap, delegates them to drivers over IPC |
| `services/displaysrv` | Display server: owns the framebuffer, composites client surfaces delivered as shared-memory capabilities, and routes input — a key to whoever claimed the keyboard, a pointer to whatever is under it, in that surface's own coordinates |
| `services/inputsrv` | virtio-input driver: two devices — a keyboard and a tablet — with their virtqueues, interrupts and event decoding all in EL0. It normalises a pointer position into a fraction of the device and never learns the size of the screen |
| `services/fssrv` | File server: owns the initramfs, answers `Open`/`Read`/`Stat`/`List`/`Close` over IPC through a client-supplied shared buffer |
| `services/fsclient` | A process with no archive and no device, reading a file anyway — the only way "these bytes arrived over IPC" means anything |
| `services/hello-c` | A program written in **C**, compiled by clang and linked against `crates/staros-libc` — the toolchain Qt will arrive through, exercised by something small enough to debug: formatting, mathematics, number parsing, the heap, `mmap`, the calendar, the clock, files through `FILE*`, a directory listing over a flat archive, the process layer, four threads with their own TLS, `poll`, and four more threads that each pin themselves to a different core and check they stayed there |
| `services/qstaros` | The **QPA plugin**: `QPlatformIntegration`, `QPlatformScreen`, `QPlatformWindow` over a `displaysrv` surface, and a `QPlatformBackingStore` that is a `QImage` over the shared pixels — no copy between `QPainter` and the compositor. Input arrives on the same `poll` the event loop already runs, through a `QSocketNotifier` on the compositor's event endpoint, and becomes `handleMouseEvent`/`handleKeyEvent` |
| `services/hello-cpp` | A program written in **C++** with the real standard library: `std::vector<std::string>`, `std::sort`, three `std::thread`s under a `std::mutex`, a namespace-scope constructor, a function-local static whose destructor runs at exit, and nine `dynamic_cast`s over all three of the ABI's type-information shapes |
| `services/qt-hello` | A program written with **Qt**: `QGuiApplication`, a `QRasterWindow` painted with `QPainter`, text in a font read from the initramfs, and an event loop that ends by itself |
| `services/shell` | A program written in **QML**: a `QQuickView` over a scene read off the filesystem at run time, rendered by Qt Quick's software adaptation — no OpenGL, no JIT, every QML module linked in and imported by name. Its `MouseArea` and `Keys.onPressed` are the far end of the input path, and it prints how many of each arrived |

All ten EL0 programs are built by `crates/kernel/build.rs` and embedded in the
kernel image; there is no separate build step. The C and C++ ones are skipped, with
a warning, on a host without clang or the C++ standard headers. The two Qt ones are
*taken* rather than built — `scripts/qt-link.sh` produces them, because Qt is a
separate tree outside this repository — and the kernel reports their absence at boot
instead of pretending they ran.

The C++ half deserves a note, because there is no `libc++` here and none was built:
the standard library's *templates* come from the host's libstdc++ headers compiled
against this tree's own C headers (`crates/staros-libc/include`), and its *compiled*
half — `operator new`, the `__cxa_*` ABI hooks, the `__throw_*` helpers a
`-fno-exceptions` build still calls, and the three out-of-line functions behind
`std::thread` — is written in `crates/staros-libc/cxx/runtime.cpp`. That list was
not designed; the linker named every symbol in it, one at a time.

## Prerequisites

The workspace pins **nightly** via `rust-toolchain.toml`; `rustup` installs it
(with `rust-src`, `llvm-tools`, `rustfmt`, `clippy`) automatically on first use.
Booting also needs QEMU:

```bash
# Arch Linux
sudo pacman -S qemu-system-aarch64
```

## Build & run

The default target is the bare-metal `aarch64-unknown-none`. The `kbuild`/`krun`
aliases add `build-std` so `core`/`alloc` are rebuilt from source (for the memory
intrinsics); prefer them over a bare `cargo build`.

```bash
cargo kbuild                 # build the kernel ELF for aarch64
cargo krun                   # build + boot it in QEMU (with a framebuffer)
cargo kclippy                # clippy across the workspace
cargo ktest-host             # portable-crate unit tests on the host (263 tests)
./scripts/smoke-test.sh      # boot the whole matrix and assert on the output
./scripts/fb-check.sh        # assert on the *pixels* the display server composited
./scripts/qml-check.sh       # assert on the *pixels* of the QML scene, then click it and assert they changed
./scripts/input-check.sh     # press a key and click on the emulated devices, and check both reached a window
./scripts/frame-profile.sh   # where a frame's time goes: scene sync, rasterising, IPC, compositing
./scripts/gdb-check.sh       # break inside an EL0 program over QEMU's gdbstub and unwind its stack
./scripts/libc-progress.sh   # score crates/staros-libc against the symbols Qt needs
./scripts/header-check.sh    # every function the sysroot declares must be one the library defines
./scripts/qpa-check.sh       # compile the QPA plugin for aarch64 against this tree's sysroot
./scripts/cxx-progress.sh    # what Qt needs from the C++ runtime, and how much of it is written
./scripts/smoke-test.sh --quick   # same, minus the slow 8-core run
```

`cargo krun` boots via the `runner` in `.cargo/config.toml`, which flattens the
ELF to an `Image` and hands it to QEMU's arm64 Linux boot stub — **the same
protocol a real bootloader uses**, and the only path that passes a device tree in
`x0`. Exit QEMU with `Ctrl-A` then `X`.

The runner builds a *complete* machine on purpose: `-device ramfb` so there is a
screen to hand to `displaysrv`, `-device virtio-keyboard-device` and
`-device virtio-tablet-device` so `inputsrv` has something to type on and something
to point with, and an initramfs built beside the image (greeting, version, and the
`init` ELF) so the archive and `SpawnImage` paths run too. Anything left out here
is a subsystem that reports "this machine has none" and vanishes from the log —
which is how the display server and then the input driver each went unnoticed after
being written. The devices cost nothing when nothing uses them.

The pointer is a *tablet* and not a mouse, and that is a decision. A tablet reports
where it is; a mouse reports how far it moved, and turning deltas into a position
needs somewhere to keep the pointer — which is the compositor, so a mouse driver
would be a second answer to where the pointer is.

If this QEMU has a graphical UI driver and there is a session to open it in, the
runner opens a **window**: the screen `displaysrv` owns, with a keyboard and a
tablet that take what is typed and clicked into it, and the serial log still coming
back to the terminal through `-serial mon:stdio`.

Arch ships those UI drivers as separate packages, and a `qemu-system-aarch64`
without them lists only `none` under `-display help`. Then there is no window, the
scene reports `0 click(s), 0 key(s)` — and the runner says so before the boot rather
than leaving that zero to be read as a broken input path. `pacman -S qemu-ui-gtk`
is what gives it a window.

Either way, `./scripts/input-check.sh` (the kernel-side path) and
`./scripts/qml-check.sh` (all the way into a `MouseArea`) synthesise real device
events over QMP and need no window at all.

`cargo krun` output — everything below is from one run, unabridged:

```
STAR OS microkernel v0.2.0 — entered at EL1, running at EL1
device tree at 0x48200000 (1048576 bytes): linux,dummy-virt
  ram: 0x40000000..0x50000000 (256 MiB)
  ram total: 256 MiB
  reserved regions: 0
  cpus: 1 (booted on cpu 0)
  intc: GICv2, dist 0x8000000, cpu 0x8010000
  psci: present, method hvc
  console: pl011 at 0x9000000 (+0x1000) — this console, found not assumed
exception vectors installed (VBAR_EL1)
privileged access never (PAN): not implemented on this CPU
MMU enabled: true (kernel in TTBR1 at 0xffff000000000000; RAM 0x40000000..0x50000000 Normal in the linear map; 44-bit PA per ID_AA64MMFR0_EL1)
memory: 256 MiB RAM, 125 MiB usable, heap 3324 KiB @ 0x48300000, 121 MiB of frames @ 0x4863f000
kernel heap: Vec of 8 squares (last 64) sums to 204 — global allocator live
dynamic tables: 64 objects (old max 8), 64 caps in one task (old max 4), 64 notifications (old max 4) — all grew past the old fixed limits
framebuffer: ramfb 640x480 online (mirroring the console to the screen)
[selftest] single-thread font render (before SMP / tasks):
  ABCDEFGHIJKLMNOPQRSTUVWXYZ
  abcdefghijklmnopqrstuvwxyz 0123456789
  !"#$%&'()*+,-./:;<=>?@[\]^_`{|}~
[selftest] SINGLE THREAD TEST PASSED
syscall Yield -> 0; syscall 0xdead -> -6
clock: 62500000 Hz counter, 16 ns per 1 tick(s) (exact)
interrupt controller: GICv2 online
clock: one tick interval (6250000 counter ticks) measured 101174 us against an expected 100000 us (agrees with the tick interval)
smp: 1 core(s) online (PSCI v1.1)
smp: 1 cores x 20000 locked increments = 20000 (expected 20000) — no increments lost
smp: single core — no inter-processor interrupt to send
loaded init ELF: 13648 bytes, entry 0x80000000
scheduler: capability delegation (client + server) + user-space IRQ driver
framebuffer: handed to displaysrv (id 14); the kernel logs to the UART from here
[fault] task 3 killed: EL0 fault at 0x40000000 (ec 0x24) — isolated, kernel continues
[fault]   backtrace (1 frames, x29 chain): 0x80000570
[devicemgr] parsed the device tree in user space: PL011 @ 0x9000000 intid 33
[devicemgr] delegated UART device+irq to the driver and a device to the server
[devicemgr] found a tablet at a003c00 intid 78 and delegated it to the input driver
[ipc] task 9 send blocked: ep7 ring full
[inputsrv] a tablet: absolute axes 0..32767 wide, reported as a fraction so the compositor keeps the screen size
[inputsrv] a keyboard
[inputsrv] virtio-input driver up in EL0: 2 device(s), queues armed, waiting
[ipc] task 9 send resumed
[devicemgr] found a keyboard at a003e00 intid 79 and delegated it to the input driver
[devicemgr] no IOMMU on this machine; DMA capability stands but is unenforced
[devicemgr] unpacked the initramfs in user space: 21 files, no storage driver
[devicemgr] read 'greeting.txt' from the initramfs: hello from the initramfs
[displaysrv] the screen is mine: kernel output stopped, pixels are a process's now
[fbclient] asked the screen its size (640x480 xRGB8888), then had two 64x64 surfaces composited - overlapping, restacked, and an 8x8 commit repainted 64 pixels and not 4096
[fssrv] the files are mine: 21 of them, served over IPC to processes that hold no archive
[fsclient] two endpoint capabilities and one page of my own memory - no archive, no device
[hello-c] a C program in EL0: printf, malloc, clock and files, no syscall in sight
[hello-c] math: sin(1e15)=0.858273, pow(1.0000001,1e7)=2.718282, hypot(3,4)=5.0
[fssrv] the files are mine: 21 of them, served over IPC to processes that hold no archive
[fssrv] the files are mine: 21 of them, served over IPC to processes that hold no archive
[stack] walked 40 pages down a stack that started with one mapped, every marker read back - pages arrived on demand
[fault] task 25 killed: EL0 fault at 0x7ffedffb0 (ec 0x24) — stack guard: growth limit reached — isolated, kernel continues
[fault]   backtrace (2 frames, x29 chain): 0x8000085c 0x80000854
[child] hello - I was created at runtime, not by the kernel
[loaded] hello - my ELF was a file in the initramfs, parsed in user space and handed to the kernel as bytes
[client] monotonic clock: two ClockNow reads from EL0, the second strictly later - no capability needed
[driver] user-space UART-RX driver waiting for input
[fsclient] stat 'greeting.txt' over IPC: 25 bytes, mode 100644
[fssrv] the files are mine: 21 of them, served over IPC to processes that hold no archive
[hello-cpp] a namespace-scope constructor ran before main
[hello-cpp] a C++ program in EL0: vector, string, thread, and a static with a destructor
[client] SleepUntil: woke no earlier than its 20 ms absolute deadline
#server drove the UART, then revoked it for everyone
[qt-hello] starting
[child] hello - I was created at runtime, not by the kernel
[client] read from shared memory: shared-memory works: written by the server, read by the client
[client] read the marker from the SECOND page of a 2-page shared buffer
[shell] starting
[dyingclient] a 32x32 window on screen, the server watching me, and now I crash
[fault] task 7 killed: EL0 fault at 0x0 (ec 0x24) — isolated, kernel continues
[fault]   backtrace (2 frames, x29 chain): 0x80000d40 0x80000d3c
[devicemgr] started 'init.elf' from the initramfs as a new process - the kernel loaded a file, not a built-in image
[devicemgr] the kernel refused a non-ELF file and an unmapped pointer, as it must
[fsclient] read 'greeting.txt' through fssrv in 2 chunks: hello from the initramfs
[fsclient] 25 of 25 bytes in 2 reads, the second one from offset 6
[hello-cpp] backing store: 1200 KiB for a whole 640x480 screen, filled and read back from C++
[client] WaitAny: index 1 of 2 from the server's notification, a lone silent source timed out, a later signal was still counted, and no stale registration poisoned the next block
[hello-cpp] a 1920x1080 backing store: 8100 KiB, contiguous, mapped whole
[hello-cpp] dynamic_cast: sibling base at +16, virtual base at +32, 9 checks over all three type-info shapes
[hello-cpp] a static local was constructed on first use
[hello-cpp] C++ RUNTIME OK - 68 strings, 600 from three threads
[hello-cpp] the static local's destructor ran at exit, holding 2 entries
[child] hello - I was created at runtime, not by the kernel
[parent] spawned 3 children via the Spawn syscall - 15 tasks total, old table held 8
[client] SpawnThread: a thread in this very address space wrote through our page and ran with its own TPIDR_EL0
[cap] task 0 denied MapMemory(handle 0): no such capability
[client] kernel refused a syscall pointer into an unmapped page - it walks our tables, not a range
[fsclient] fssrv refused an unopened handle, a missing file, a closed handle and a lied-about length
[fsclient] asked for all 13648 bytes of 'init.elf' into a 4096-byte buffer and got 4096, with 9552 left
[fsclient] the archive is at 0x900000000 in fssrv; touching it here must fault
[fault] task 14 killed: EL0 fault at 0x900000000 (ec 0x24) — isolated, kernel continues
[fault]   backtrace (3 frames, x29 chain): 0x80000014 0x800016c8 0x80000004
[fssrv] served 12 requests, 4121 bytes of file data, and refused 4 - the archive never left this address space
[ipc-storm] receiver drained every message from 3 concurrent senders, sequence sum exact - no message lost or duplicated
[displaysrv] a client died; its windows are off the screen
[qt-hello] QGuiApplication constructed, platform=staros
[qt-hello] window shown
[qt-hello] entering the event loop
[shell] QGuiApplication constructed, platform=staros
[shell] scene 'qml/Main.qml' is 7704 bytes
[shell] threads: a QThread with an 8 MiB stack ran and joined, and took a QMutex the main thread was holding: yes
[memtest] MapAnon(0) refused - a zero-page request is an error, not a page
[memtest] DMA buffer: 4 physically-contiguous non-cacheable pages, first and last written and read back
[memtest] 2.5 MiB .bss reaches 2.25 MiB in (past the 2 MiB L2 boundary); grew the heap by 16 MiB in 8 calls of 1024 pages, first and last page of every run zeroed then written and read back, runs handed out back to back
[memtest] unmapped one page and kept its address; touching it must fault
[fault] task 4 killed: EL0 fault at 0x102000000 (ec 0x24) — isolated, kernel continues
[fault]   backtrace (2 frames, x29 chain): 0x8000070c 0x8000070c
[hello-c] mmap: 12305 bytes mapped and returned, 0 retained by the kernel, 64 MiB cycled through a smaller pool
[hello-c] calendar: 2025-08-13 00:00:00 UTC (Wed)
[hello-c] heap: 103 allocations, 896 bytes live at the end
[hello-c] clock: 21935776 ns across a 20 ms nanosleep
[hello-c] read 'greeting.txt' through fssrv with libc's open/read/lseek: hello from the initramfs
[hello-c] FILE*: fgetc/ungetc/fgets/fread agree with ftell
[qt-hello] painted 320x240, text in 'IBM Plex Mono' 115 px wide, 20 px tall
[qt-hello] event loop tick 1
[qt-hello] quitting
[qstaros] present: 1 frame(s), 76800 px - commit 1018 us/frame, post 1018 us/frame, await 0 us/frame, restore 0 us/frame - worst post 1018 us, worst await 0 us, worst restore 0 us, 13 ns/px committed
[qt-hello] exec returned 0
[fssrv] served 233 requests, 688420 bytes of file data, and refused 11 - the archive never left this address space
[hello-c] listed 'docs': 1 file, 1 directory, over a flat archive
[hello-c] sendfile: from the initramfs
[hello-c] process 16: uname StarOS 0.2.0, stack limit 1024 KiB, backtrace 3 frames
[shell] scene loaded, root is a Main_QMLTYPE_0 of 320x300
[displaysrv] a client died; its windows are off the screen
[shell] view shown and the keyboard claimed
[shell] entering the event loop
[displaysrv] composite: 128 frame(s), 1673994 px in 370274 us - 2892 us/frame, 13078 px/frame, 221 ns/px, worst 44669 us for 96000 px
[displaysrv] composite: 256 frame(s), 3306810 px in 650545 us - 2541 us/frame, 12917 px/frame, 196 ns/px, worst 44669 us for 96000 px
[shell] 312 frame(s) in 15279 ms
[shell] frame profile over 312 frame(s): sync 8338 us, raster 28718 us, present 1180 us per frame; worst frame 616761 us
[shell] between frames over 311 gap(s): total 9993 us, asleep 8486 us, awake 1507 us, parks 1.00 per gap
[shell] the animated rectangle moved from x=275.4 to x=122.8
[shell] input: 0 click(s) reached a MouseArea, 0 key(s) reached the scene, the last was Qt key 0
[qstaros] present: 312 frame(s), 4037274 px - commit 1057 us/frame, post 441 us/frame, await 616 us/frame, restore 142 us/frame - worst post 6371 us, worst await 41589 us, worst restore 1454 us, 81 ns/px committed
[shell] exec returned 0
[fssrv] served 371 requests, 696124 bytes of file data, and refused 23 - the archive never left this address space
[hello-c] threads: 4 workers x 250 increments = 1000, 1 thread(s) live at the end
[hello-c] affinity: 1 core(s) online, main pinned to cpu0 and was seen on 1 core(s), 1 worker(s) on 1 distinct core(s), an absent core refused
[hello-c] classes: over 1200 ms on one core, latency took 205528755 turn(s) and bulk 29991679 - 6x, and neither was starved
[hello-c] capabilities: a handle minted after a thread was running resolved inside it, 8192 bytes - one table per address space, not per task
[hello-c] poll: a thread slept on an eventfd and a pipe, and a 20 ms timeout took 22041744 ns
[hello-c] endpoint in poll: a message from another process woke the loop in 3236432 ns, and a 20 ms bounded receive on an empty one gave up after 22825904 ns
[hello-c] shared buffers: 16 KiB of surface, mapped at 0x500001000 and 0x500005000
[displaysrv] a client died; its windows are off the screen
[displaysrv] composited client surfaces onto a screen no client can touch
[displaysrv] 4 surface(s) live, 322 commit(s), 4484330 pixel(s) composited, 8 refused, 3 client(s) reaped, 0 input event(s) routed, 0 dropped for want of a window
[displaysrv] buffers: 2, 330 flip(s), 0 refused - 5702210 px painted for 4791530 px of damage, 19% over
[displaysrv] composite: 322 frame(s), 4131306 px in 811748 us - 2520 us/frame, 12830 px/frame, 196 ns/px, worst 48095 us for 2378 px
[hello-c] window: a 48x48 surface on a 640x480 screen, double buffered, from C through staros.h
[hello-c] font: read 133796 bytes of IBM Plex Mono through fssrv, checksum 6016661948058288260
[hello-c] ctype: fourteen classifications the header had promised and nobody had written
[hello-c] stdlib: qsort, bsearch, rand, strdup and the special functions this sysroot had only promised
[hello-c] C RUNTIME OK - every check passed
[fssrv] served 248 requests, 275941 bytes of file data, and refused 11 - the archive never left this address space
clock: the demo took 55908 ms on the monotonic clock, during which core 0 took 977 tick(s)
sleep: 7 task-sleep(s) parked, 1 deadline(s) already past (returned at once), 321 clock wake-up(s), worst overshoot 13027 us
scheduler: all tasks finished after 977 timer ticks; task table grew to 46 (old fixed max 8)
task teardown: reaped 42 dead-task kernel stacks (1344 KiB returned to the heap)
user stacks: 65 page(s) mapped on demand (260 KiB), 1 mapped up front per task, limit 1024 KiB
preemption: timer ticks per core — cpu0=977
scheduling: 1893 context switch(es) over 1 core(s) — cpu0=1893
kernel heap: peak 1215 of 3324 KiB (539 KiB still in use), 0 allocation(s) refused — room for 103 task stack(s) at once
kernel heap: 508 call(s), 1893 alloc step(s) and 1524 dealloc step(s) over the free list — 6 step(s) per call
spawn: nothing was refused for want of a resource
scanout: 2 buffer(s), 330 flip(s), 0 refused, showing buffer 0 at the end — every flip taken
classes: 4 declaration(s) — latency 378, normal 4917, bulk 2; 103 preempt(s) declined, 675 fair pick(s), 10 inversion(s) — within the fairness budget
affinity: 5 pin(s), 0 forced migration(s), 3 task(s) still pinned
affinity:   task 41 pinned to 0x1, ran on 0x1 — stayed inside its mask
affinity:   task 42 pinned to 0x1, ran on 0x1 — stayed inside its mask
affinity:   task 43 pinned to 0x1, ran on 0x1 — stayed inside its mask
ipc storm: 192 sends / 192 recvs on one endpoint — cpu0=192s/192r (1 core(s) sending, 1 receiving) — endpoint exercised on one core
console mirror: 200 byte(s) painted, 0 timed at 0 us (0 ns/byte, scroll included)
shared memory: an 8-page buffer assembled from 8 run(s) of frames out of a pool holed on purpose — contiguity is no longer required, only DMA needs it
frame reclaim: post-teardown alloc 0x4864f000 (exited client's root was 0x48640000)
frame reclaim: longest free run 32 MiB -> 16 MiB after teardown — short only by what the tasks below still hold
  (4 task(s) still alive and holding their address space — send a newline to let the UART driver exit and the pool returns whole)
    task 2 (pid 2): blocked (waiting for a message)
    task 6 (pid 6): blocked (waiting for a message), recv on ep24
    task 8 (pid 8): blocked (waiting for a message)
    task 11 (pid 11): blocked (waiting for a message)
shutting down (PSCI SYSTEM_OFF)
```

## Testing

Two layers, deliberately different in kind:

- **`cargo ktest-host`** — 263 tests over the portable crates (`abi`, `hal`,
  `cpio`, `fdt`, `framebuffer`, `videocore`, `virtio`, `iommu`, `mm`, `ipc`,
  `staros-libc`, `init`), including `fdt` against real `.dtb` blobs, `cpio` against a
  real archive, `hal`'s tick↔nanosecond arithmetic against the frequencies real
  machines report, the C library's format engine, allocator and string
  functions, and the scheduler's pick policy — the affinity mask arithmetic, the
  round-robin scan, which is where an off-by-one turns into "the pinned task ran
  somewhere else, sometimes", and the class bands, where the starvation an escape
  is supposed to prevent hides one band up. Fast, and they cover the code whose bugs
  are silent.
- **`./scripts/smoke-test.sh`** — builds one image and boots it across the machine
  matrix (GICv2 smp1, GICv2 smp4, GICv3 smp4, 128 MiB, `ramfb`, SMMU, and an
  8-core run without `--quick`), asserting on expected lines *and* the absence of
  failure signals. Currently **334 assertions, exit=0** across the full matrix.
- **`./scripts/input-check.sh`** — the only check that makes the *outside world*
  act: QEMU synthesises a real key press and a real click, and the assertions are
  that a driver in EL0 decoded them with no kernel code anywhere in the path, and
  that each reached the right window by a *different* rule — the key by who claimed
  the keyboard, the click by what was under the pointer. A compositor routing clicks
  by focus passes every other check in this list and delivers every click to the
  wrong window.
- **`./scripts/fb-check.sh`** — the only check that cares what the screen *looks*
  like: it boots with `ramfb`, screendumps over QMP, and asserts named coordinates
  (the client's surface where it asked for it, the server's background around it,
  no kernel console text left). A compositor that ignores its client's coordinates
  passes every text assertion above and fails this one.

  *Which* frame it judges is half the check, and getting that wrong cost this check
  its truth twice. Judging the first frame that passed hid a window that closed
  without repainting what it covered, because every frame before that window existed
  passes. Judging the last frame before power-off replaced it with a race: the C
  program's back-buffer swap is one of the last things composited and the machine
  stops tens of milliseconds later, so a screendump landing in that gap is empty,
  gets discarded, and the frame judged is the one *before* the swap — a real failure
  report, `(301,301) is (0,255,0)`, about a run whose final frame was the correct
  `(0,200,0)`. It now waits for the display server's own profile line, which it
  prints after its last composite and before it exits, and judges a frame taken
  after that. Nothing draws afterwards, so there is nothing left to race.
- **`./scripts/qml-check.sh`** — the same tool pointed at the layer above: it boots
  with `ramfb` *and* the archive, and looks for the QML scene. Not for coordinates
  this time but for the scene's shape — a solid red run wide enough to be the card
  rather than a stray pixel, white inside it where the border and the glyphs are,
  and the two animated items present. It is the only check that can answer this
  phase's criterion, which was written as "`Rectangle { color: "red" }` is visible
  on the screen" and which no line of text can settle. Falsifying it is one line:
  freeze `ClockNow` and the scene never appears at all.

  It is also the only check that performs an *experiment* rather than an
  observation: having found the scene, it locates the orange button by colour,
  synthesises a click at its centre on the emulated tablet, and reads the pixels
  back to see it turn green. The target is found rather than written down, so the
  compositor's hit test is under test at the same time — a server that routed by
  anything other than where the surface actually is misses by exactly the window's
  offset, and the button stays orange.
- **`./scripts/frame-profile.sh`** — the only one that asks *how long*, and the
  measurement phase G8 opens with. A frame is not visible to any one process, so it
  is timed on three sides at once and the script assembles them: the QML program
  times the scene-graph stages from the signals the software render loop emits, the
  QPA plugin times the commit round trip and the back-buffer restore, and the display
  server times its own compositing and reports nanoseconds per pixel. The
  interesting figures are the differences — the round trip minus the compositing
  inside it is what the kernel's IPC costs, and nothing measures that directly. It
  fails if any of the three stages went unmeasured, because a profile that quietly
  reports two of three is how "rasterising is free" gets believed.
- **Affinity** is checked from both sides at once, and neither side alone would be
  enough. From EL0, `hello-c` spawns one thread per online core, pins each to its
  own, and asserts that across sixty-four reschedules each was seen on exactly one
  core and no two shared one — which is also what catches a per-process affinity,
  since inherited masks would put every thread on `main`'s core. From EL1, the
  kernel prints each pinned task's mask beside the set of cores that actually ran
  it, and that half survives a host without clang, where there is no C program to do
  the asking. Falsifying it is one line: take the mask test out of
  `Scheduler::pickable` and the kernel says `RAN OUTSIDE ITS MASK` on three of four
  tasks while six named assertions fail in the C program. A mask reads back exactly
  as written whether or not anything consults it.
- **Scheduling classes** are checked from both sides too, and for the same reason
  affinity is: a class is a field, and a field reads back exactly as written whether
  or not the picker consults it. From EL0, `hello-c` pins two threads that never
  block to core 0 — one latency-class, one bulk — and asserts both that the latency
  one got most of the core and that the bulk one got *some* of it. Those are two
  different defects: a picker that ignores the band lands near one to one, and one
  with strict bands and no escape reads exactly zero for the bulk thread, which is a
  kernel any thread can stop by declaring itself urgent and then looping. From EL1,
  the kernel counts *inversions* — claims that took a task while a runnable task of
  a more urgent band was available to the same core — with a scan that does not
  share a line with the picker, and prints them beside the number of fairness picks.
  An inversion is reachable on a fairness pick and on no other kind, so one can
  never exceed the other, and that half also survives a host without clang. It is
  the half that catches removing the band loop outright: `728 inversion(s)` against
  `671 fair pick(s)`, and the verdict reading `PICKED BELOW A WAITING HIGHER CLASS`.
  Setting the EL0 margin against that same run found a weak assertion — with the
  bands gone but the refusal to switch down still in, the split is 2.26 to one,
  which a threshold of two to one had been letting through.
- **Double buffering** is checked by the two things that can each be true while the
  other is false. That the *picture* is right is `fb-check.sh`'s job and it did not
  need changing: remove the per-buffer staleness the second buffer needs and it
  fails on a pixel — `surface pixel (101,81) is (24, 24, 32), not red`, the buffer
  on show never having received that surface's paint. That the *display actually
  moved* is the kernel's, because only the kernel performs a flip: two buffers with
  zero flips reserves twice the memory, shows one half of it for the whole boot and
  looks identical in every screenshot, so the shutdown line says
  `THE SECOND BUFFER WAS NEVER SHOWN` and the matrix forbids that on every machine.
- **The kernel heap** is asserted by its high-water mark, which is the only figure
  a fixed heap can be judged by: `used` at the end of a boot is nearly meaningless,
  because everything has been torn down by then, and it was the only number this
  kernel could produce. A C++ program aborting on some runs and not others turned
  out to be that heap at 95 % — `peak 1219 of 1276 KiB`, less than two 32 KiB task
  stacks of headroom — with all four ways a spawn can fail returning the same
  `OutOfResources` and no line naming any of them. The matrix now asserts the line
  on every machine and forbids `SPAWN(S) REFUSED`.
- **`./scripts/gdb-check.sh`** — boots with QEMU's gdbstub, breaks inside an EL0
  program and asserts that gdb unwinds to *that program's* caller. It is the check
  for the debugger itself, which matters from here on: the code arriving in EL0 was
  no longer all written in this tree.

Two tools rather than checks, both for reading a crash:

- **`./scripts/symbolize.sh <program>`** turns the addresses in a `[fault]
  backtrace` line back into function names, from the unstripped copy of the image
  the build keeps beside the stripped one. Pipe a whole boot log through it.
- **`./scripts/libc-progress.sh`** scores `crates/staros-libc` against
  `docs/libc-contract.txt` — the list of C library symbols a real Qt 6 build leaves
  undefined, so "how far along is the libc" has a number instead of an opinion.

## Status

Phases 1 and 2 of [`docs/ROADMAP-PIXEL.md`](docs/ROADMAP-PIXEL.md) are closed and
verified live in QEMU. Phase 3 is the watershed — first real hardware, a
**Raspberry Pi 5** — and its software groundwork is already in the tree (FDT,
framebuffer console, VideoCore mailbox, GICv2, PSCI/SMP). What remains there
needs the board, not more code: `./scripts/pi5-sdcard.sh <mounted-boot-part>`
stages a card, and [`docs/PI5-BRINGUP.md`](docs/PI5-BRINGUP.md) is the checklist
— what to verify before the first boot, and how to read each failure mode.

The graphical stack is planned separately, in
[`docs/ROADMAP-QML.md`](docs/ROADMAP-QML.md): what a QML UI actually demands of a
microkernel, which of those pieces already exist here, and the ABI gaps that block
a Qt event loop long before any Qt code enters the tree. Its phase G5 — the C and
C++ runtime — is closed: the contract is **282 of 282 symbols**, measured by
`./scripts/libc-progress.sh` against the list a real Qt 6 build leaves undefined.
That number counts symbols the archive defines, not operations this system has:
`fork` answers `ENOSYS` and writing to a file answers `EROFS`, each decision
written down with its reason in
[`docs/LIBC-CONTRACT.md`](docs/LIBC-CONTRACT.md).

Since then that roadmap's G6 and G7 have closed too: the QPA plugin links, a QML
scene renders through Qt Quick's software adaptation onto the compositor's screen,
and — as of G7.1 — a click on an emulated tablet reaches a `MouseArea` inside that
scene while a keystroke reaches the same scene by the other route. Both are proved
by pixels rather than by log lines: `./scripts/qml-check.sh` finds the button by its
colour, clicks it, and reads back the colour it became. What is left in that roadmap
is speed (G8) and the board (G9).

G8 opens with a measurement, and the measurement has been taken:
`./scripts/frame-profile.sh` decomposes a frame into rasterising **63 %**, scene
synchronisation **20 %** and the flush to the display server **16 %**, with a further
20 % of the wall-clock interval outside the render loop entirely. Inside that flush,
compositing costs 1 060 µs against a 3 762 µs round trip — the other 2 700 µs is
kernel IPC. The copy into the framebuffer, which that roadmap named as the likely
cause of a slow frame, is **2.9 %** of it. The numbers are TCG numbers and do not
transfer to a board; the proportions held across three runs, and they are what
reordered the rest of the phase.

That measurement named the one number in a frame that was ours, and it has since been
acted on. The commit is no longer waited for inside the frame: the plugin posts it and
collects the answer at the top of the next paint, where the answer means the display
server has let go of the buffer about to be painted. The flush fell from **15.8 % of a
frame to 3.1 %**, a round trip from 5 112 µs to 981, and the server now spends longer
compositing than the client spends in the whole round trip — which is the overlap,
stated as a number. Waiting was not removed; it stopped being in the frame.

The last unmeasured fifth of a frame is measured too, and it was not what the label
said. The gap between one frame and the next — 7 427 µs of a 43 056 µs interval — is
**81 % sleep**: the event loop parks in `poll` exactly once, waits for the next
animation tick, and wakes. The work in there is 1 412 µs. The counter lives in the C
library (`staros_poll_wait`) rather than in the toolkit, because `poll` is the only
call that parks an event loop here and time *asleep* is a different number from time
*in the call* — a poll that finds a descriptor ready never parks. The frame is not
waiting on a timer; it is waiting on rasterising, which is Qt's.

Two of the checks turned out to be measuring the host rather than the system, and both
are fixed: `input-check.sh` pressed its keys on a fifteen-second timer that the boot
outgrew, and now waits for the driver to say its queues are armed; `qml-verify` asked
whether an animated bar was wide in the one frame where an unrelated card was fullest,
and now asks every frame.

Away from the graphical stack, the kernel's own debt list lost another entry:
affinity. A task can now name the cores it may run on (`SetAffinity`) and ask which
one it is on (`CpuId`), and under that a preference the picker applies to tasks that
asked for nothing — each core reaches first for a task it ran before, and falls
through to anything runnable rather than idling beside it. The policy itself is a
pure function with host tests, and cutting it out found a latent defect in the scan
it replaced: the kernel's round-robin was `(1..=n)`, whose last offset wraps back
onto the slot it started from, so it could return the very task the caller was
switching away from. Five call sites happened to make that unreachable; the sixth,
which needed to leave a task runnable while looking for its successor, would have had
it switch to itself.

The preference cost a boot to get right, and the way it failed is the part worth
keeping. A task that has never run has no last core, so comparing `last_cpu == cpu`
made a *fresh* task match on no core at all — reachable only by the fall-through,
that is, only when nothing which had already run was runnable. A newly created thread
never started, and what said so was `[client] THREAD WRONG` on the single-core
machine: a check written long before affinity existed, for an entirely different
failure, and the only one in the matrix that waits on a thread with a deadline. The
host tests all passed, because each described the preference as intended rather than
as written. A task that has never run is now at home on every core, which is also the
right answer on the merits — it has left cache lines nowhere, so there is nothing to
keep it near and nothing to defer it for.

The next entry off that list is the one affinity could not answer: *in what order*.
`SetClass` puts a task in one of three bands — latency, normal, bulk — and the
display server and the input driver now declare the first, which is the whole
motivation stated as two lines of service code. The policy is `affinity::choose` run
once per band, so nothing about the round-robin order or the sticky pass is
re-derived; the only thing added is which slots the scan may see.

**Ordering the candidates turned out to be half of a priority, and the measurement
is what said so.** Two threads on one core, one latency and one bulk, neither ever
blocking: the first result was `68490073` turns against `61142525`, which is one to
one. The picker was obeying its bands perfectly and it changed nothing, because a
timer preempt asks who *else* may run and the scan excludes the task being switched
away from — so the only candidate on that core was the bulk thread, every tick. The
other half is declining to leave at all: an involuntary preempt that would hand the
core to a strictly less urgent task is refused now, and the same two threads split
it `186459665` to `14882258`.

The escape that keeps that from being a hang cost a boot of its own, and moved a
bug rather than fixing it the first time. One decision in eight, per core, reverses
the band order — and reversing it means starting at the *bottom*, which is what the
first version did. With the latency thread spinning, every one of those picks went
to the bulk thread and the middle band never ran at all; the normal-class thread
waiting to stop the measurement never woke, so the spinner never stopped, so the
boot never finished. An escape that always serves the bottom does not prevent
starvation, it moves it one band up where there is nothing watching for it.
Fairness rounds now alternate between the middle and bottom bands.

Back on the graphical side, G8's third item is closed: the screen is double
buffered. The pixels were always user space's — the kernel maps them into the
display server and stops drawing — but the *scanout base* is a register behind
fw_cfg on QEMU and a GPU mailbox on a Pi, and there is no page of it to hand over.
So the kernel allocates the buffers stacked in one run, maps them all, and choosing
between them is a syscall through a capability that names no address at all.

The interesting part is not the flip. It is that **damage tracking and a second
buffer are each correct alone and wrong together**, silently. Repainting only what
changed is exact with one buffer, because that buffer holds every earlier frame;
with two, the one being drawn into is a frame behind, so this frame's damage leaves
the previous frame's unrepaired and the screen shows stale pixels every other
frame. Staleness is therefore tracked per buffer. Removing that is a pixel failure
and not a log one — `surface pixel (101,81) is (24, 24, 32), not red` — which is
the whole reason `fb-check` exists.

The cost is measured rather than argued: under the QML workload the server painted
**5 685 332** pixels for **4 790 080** of damage, 18 % over, across 327 flips with
none refused. On the sparse `fb-check` workload — ten commits rather than hundreds
of frames — the same code came out at 91 % and 134 % on two runs, because staleness
is a bounding rectangle and the overhead there is dominated by whichever pair of
damages happened to straddle the screen. What none of this shows is an absence of tearing: `ramfb` has no
vertical blank and neither does QEMU, so a flip lands at once rather than at a
scanline boundary. What is shown is that the compositor never writes the buffer
being read. Tearing is a claim for a board, and G8's vsync item stays open for the
same reason.

The last thing this round closed was not on any list, because nothing knew it was
there. A C++ program aborted on some boots and not others —
`thread::_M_start_thread: the kernel refused another thread` — and the kernel said
nothing at all: `SpawnThread` had four ways to fail and all four returned the same
error with no line anywhere, so which resource had run out was not merely unlogged
but unknowable.

The instrument was the fix. The allocator now counts bytes in use, refusals, and
above all the **peak**, because a fixed heap is exhausted at an instant and half
empty a moment later — the allocation returns null, the caller turns it into an
error of its own, and by the time anything asks how full the heap was, it is not
full any more. The first measurement settled the intermittent in one line: **`peak
1219 of 1276 KiB`**, 95 % full, with less than two of the 32 KiB kernel stacks that
every task takes out of that region. The ceiling was thirty-nine simultaneously
live tasks and the demo reaches forty-six; whether a boot crossed it depended on
scheduling order and nothing else.

Deliberately starving the heap then found a second defect, better than the first.
Ten allocations were refused, `spawn: no thread was refused`, and half the boot's
programs never started — the instrument had been fitted to threads and not to
processes. With both named, the allocator's count and the scheduler's named causes
agree exactly, ten and ten, which is the claim that no refusal goes unattributed:
they are counted by different code in different crates, so a silent path elsewhere
would show as a gap between them.

The sizing then followed from the number instead of from a round figure — the heap
holds sixty-four kernel stacks plus the old megabyte for everything that is not a
stack — and the same workload now peaks at **`1222 of 3324 KiB`**, 37 %, with room
for 103 task stacks. On the 128 MiB machine, the smallest in the matrix, that heap
is 2.4 % of RAM and peaks at 778 KiB.

Design rationale, the SMP/IPC/IOMMU write-ups, and an honest "not yet
implemented" list live in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).
