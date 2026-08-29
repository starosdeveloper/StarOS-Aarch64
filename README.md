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
address space, processes loaded from a file rather than from the kernel image, a
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
| `crates/abi` | Syscall numbers, error codes, capability `Handle` — the kernel↔user contract |
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
| `services/hello-c` | A program written in **C**, compiled by clang and linked against `crates/staros-libc` — the toolchain Qt will arrive through, exercised by something small enough to debug: formatting, mathematics, number parsing, the heap, `mmap`, the calendar, the clock, files through `FILE*`, a directory listing over a flat archive, the process layer, four threads with their own TLS, and `poll` |
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
cargo ktest-host             # portable-crate unit tests on the host (221 tests)
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
memory: 256 MiB RAM, 125 MiB usable, heap 1276 KiB @ 0x48300000, 123 MiB of frames @ 0x4843f000
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
clock: one tick interval (6250000 counter ticks) measured 101041 us against an expected 100000 us (agrees with the tick interval)
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
[displaysrv] the screen is mine: kernel output stopped, pixels are a process's now
[fssrv] the files are mine: 21 of them, served over IPC to processes that hold no archive
[fsclient] two endpoint capabilities and one page of my own memory - no archive, no device
[fssrv] the files are mine: 21 of them, served over IPC to processes that hold no archive
[hello-c] a C program in EL0: printf, malloc, clock and files, no syscall in sight
[hello-c] math: sin(1e15)=0.858273, pow(1.0000001,1e7)=2.718282, hypot(3,4)=5.0
[fssrv] the files are mine: 21 of them, served over IPC to processes that hold no archive
[fssrv] the files are mine: 21 of them, served over IPC to processes that hold no archive
[hello-cpp] a namespace-scope constructor ran before main
[hello-cpp] a C++ program in EL0: vector, string, thread, and a static with a destructor
[stack] walked 40 pages down a stack that started with one mapped, every marker read back - pages arrived on demand
[fault] task 25 killed: EL0 fault at 0x7ffedffb0 (ec 0x24) — stack guard: growth limit reached — isolated, kernel continues
[fault]   backtrace (2 frames, x29 chain): 0x8000080c 0x80000804
[child] hello - I was created at runtime, not by the kernel
[client] monotonic clock: two ClockNow reads from EL0, the second strictly later - no capability needed
[driver] user-space UART-RX driver waiting for input


[driver] newline received; user-space IRQ driver exiting
[inputsrv] a tablet: absolute axes 0..32767 wide, reported as a fraction so the compositor keeps the screen size
[inputsrv] a keyboard
[inputsrv] virtio-input driver up in EL0: 2 device(s), queues armed, waiting
[ipc] task 9 send resumed
[devicemgr] found a keyboard at a003e00 intid 79 and delegated it to the input driver
[devicemgr] no IOMMU on this machine; DMA capability stands but is unenforced
[devicemgr] unpacked the initramfs in user space: 21 files, no storage driver
[devicemgr] read 'greeting.txt' from the initramfs: hello from the initramfs
[fsclient] stat 'greeting.txt' over IPC: 25 bytes, mode 100644
[hello-c] mmap: 12305 bytes mapped and returned, 16384 retained by the kernel
[hello-c] calendar: 2025-08-13 00:00:00 UTC (Wed)
[hello-c] heap: 103 allocations, 896 bytes live at the end
[loaded] hello - my ELF was a file in the initramfs, parsed in user space and handed to the kernel as bytes
[devicemgr] started 'init.elf' from the initramfs as a new process - the kernel loaded a file, not a built-in image
[devicemgr] the kernel refused a non-ELF file and an unmapped pointer, as it must
[hello-c] clock: 529210256 ns across a 20 ms nanosleep
[qt-hello] starting
[child] hello - I was created at runtime, not by the kernel
[client] SleepUntil: woke no earlier than its 20 ms absolute deadline
#server drove the UART, then revoked it for everyone
[dyingclient] a 32x32 window on screen, the server watching me, and now I crash
[fault] task 7 killed: EL0 fault at 0x0 (ec 0x24) — isolated, kernel continues
[fault]   backtrace (2 frames, x29 chain): 0x80000cf0 0x80000cec
[displaysrv] a client died; its windows are off the screen
[shell] starting
[hello-cpp] backing store: 1200 KiB for a whole 640x480 screen, filled and read back from C++
[child] hello - I was created at runtime, not by the kernel
[client] read from shared memory: shared-memory works: written by the server, read by the client
[client] read the marker from the SECOND page of a 2-page shared buffer
[fsclient] read 'greeting.txt' through fssrv in 2 chunks: hello from the initramfs
[fsclient] 25 of 25 bytes in 2 reads, the second one from offset 6
[hello-cpp] a 1920x1080 backing store: 8100 KiB, contiguous, mapped whole
[hello-cpp] dynamic_cast: sibling base at +16, virtual base at +32, 9 checks over all three type-info shapes
[hello-cpp] a static local was constructed on first use
[hello-cpp] C++ RUNTIME OK - 68 strings, 600 from three threads
[hello-cpp] the static local's destructor ran at exit, holding 2 entries
[client] WaitAny: index 1 of 2 from the server's notification, a lone silent source timed out, a later signal was still counted, and no stale registration poisoned the next block
[parent] spawned 3 children via the Spawn syscall - 15 tasks total, old table held 8
[client] SpawnThread: a thread in this very address space wrote through our page and ran with its own TPIDR_EL0
[cap] task 0 denied MapMemory(handle 0): no such capability
[client] kernel refused a syscall pointer into an unmapped page - it walks our tables, not a range
[fbclient] asked the screen its size (640x480 xRGB8888), then had two 64x64 surfaces composited - overlapping, restacked, and an 8x8 commit repainted 64 pixels and not 4096
[fsclient] fssrv refused an unopened handle, a missing file, a closed handle and a lied-about length
[hello-c] read 'greeting.txt' through fssrv with libc's open/read/lseek: hello from the initramfs
[fsclient] asked for all 13648 bytes of 'init.elf' into a 4096-byte buffer and got 4096, with 9552 left
[fsclient] the archive is at 0x900000000 in fssrv; touching it here must fault
[fault] task 14 killed: EL0 fault at 0x900000000 (ec 0x24) — isolated, kernel continues
[fault]   backtrace (3 frames, x29 chain): 0x80000014 0x800016c8 0x80000004
[fssrv] served 12 requests, 4121 bytes of file data, and refused 4 - the archive never left this address space
[hello-c] FILE*: fgetc/ungetc/fgets/fread agree with ftell
[displaysrv] pointer at (101, 119) landed on surface 3 of client 0 - routed by what is under it, not by who has the keyboard
[ipc-storm] receiver drained every message from 3 concurrent senders, sequence sum exact - no message lost or duplicated
[qt-hello] QGuiApplication constructed, platform=staros
[shell] QGuiApplication constructed, platform=staros
[qt-hello] window shown
[qt-hello] entering the event loop
[shell] scene 'qml/Main.qml' is 7704 bytes
[shell] threads: a QThread with an 8 MiB stack ran and joined, and took a QMutex the main thread was holding: yes
[memtest] MapAnon(0) refused - a zero-page request is an error, not a page
[memtest] DMA buffer: 4 physically-contiguous non-cacheable pages, first and last written and read back
[memtest] 2.5 MiB .bss reaches 2.25 MiB in (past the 2 MiB L2 boundary); grew the heap by 16 MiB in 8 calls of 1024 pages, first and last page of every run zeroed then written and read back, runs handed out back to back
[hello-c] listed 'docs': 1 file, 1 directory, over a flat archive
[hello-c] sendfile: from the initramfs
[hello-c] process 16: uname StarOS 0.2.0, stack limit 1024 KiB, backtrace 3 frames
[qt-hello] painted 320x240, text in 'IBM Plex Mono' 115 px wide, 20 px tall
[qt-hello] event loop tick 1
[qt-hello] quitting
[qstaros] present: 1 frame(s), 76800 px - commit 108738 us/frame (worst 108738 us), restore 409 us/frame (worst 409 us), 1415 ns/px committed
[qt-hello] exec returned 0
[fssrv] served 233 requests, 688420 bytes of file data, and refused 11 - the archive never left this address space
[shell] scene loaded, root is a Main_QMLTYPE_0 of 320x300
[displaysrv] a client died; its windows are off the screen
[shell] view shown and the keyboard claimed
[shell] entering the event loop
[hello-c] threads: 4 workers x 250 increments = 1000, 1 thread(s) live at the end
[hello-c] poll: a thread slept on an eventfd and a pipe, and a 20 ms timeout took 33281008 ns
[hello-c] endpoint in poll: a message from another process woke the loop in 40577424 ns
[hello-c] shared buffers: 16 KiB of surface, mapped at 0x500001000 and 0x500005000
[hello-c] window: a 48x48 surface on a 640x480 screen, double buffered, from C through staros.h
[hello-c] font: read 133796 bytes of IBM Plex Mono through fssrv, checksum 6016661948058288260
[hello-c] ctype: fourteen classifications the header had promised and nobody had written
[hello-c] stdlib: qsort, bsearch, rand, strdup and the special functions this sysroot had only promised
[hello-c] C RUNTIME OK - every check passed
[fssrv] served 248 requests, 275941 bytes of file data, and refused 11 - the archive never left this address space
[displaysrv] composite: 128 frame(s), 1603666 px in 135778 us - 1060 us/frame, 12528 px/frame, 84 ns/px, worst 5906 us for 76800 px
[displaysrv] composite: 256 frame(s), 3270992 px in 270530 us - 1056 us/frame, 12777 px/frame, 82 ns/px, worst 5906 us for 76800 px
[displaysrv] composite: 384 frame(s), 4870516 px in 428679 us - 1116 us/frame, 12683 px/frame, 88 ns/px, worst 5906 us for 76800 px
[shell] 374 frame(s) in 14707 ms
[shell] frame profile over 374 frame(s): sync 6546 us, raster 19975 us, present 4669 us per frame; worst frame 283637 us
[shell] the animated rectangle moved from x=77.1 to x=79.3
[shell] input: 0 click(s) reached a MouseArea, 0 key(s) reached the scene, the last was Qt key 0
[qstaros] present: 374 frame(s), 4776484 px - commit 3788 us/frame (worst 21026 us), restore 96 us/frame (worst 466 us), 296 ns/px committed
[shell] exec returned 0
[displaysrv] composited client surfaces onto a screen no client can touch
[displaysrv] 4 surface(s) live, 384 commit(s), 5223540 pixel(s) composited, 8 refused, 2 client(s) reaped, 1 input event(s) routed, 17 dropped for want of a window
[displaysrv] composite: 384 frame(s), 4870516 px in 428679 us - 1116 us/frame, 12683 px/frame, 88 ns/px, worst 5906 us for 76800 px
[fssrv] served 371 requests, 696124 bytes of file data, and refused 23 - the archive never left this address space
clock: the demo took 32628 ms on the monotonic clock, during which core 0 took 1011 tick(s)
sleep: 6 task-sleep(s) parked, 1 deadline(s) already past (returned at once), 441 clock wake-up(s), worst overshoot 120752 us
scheduler: all tasks finished after 1011 timer ticks; task table grew to 42 (old fixed max 8)
task teardown: reaped 39 dead-task kernel stacks (1248 KiB returned to the heap)
user stacks: 65 page(s) mapped on demand (260 KiB), 1 mapped up front per task, limit 1024 KiB
preemption: timer ticks per core — cpu0=1011
ipc storm: 192 sends / 192 recvs on one endpoint — cpu0=192s/192r (1 core(s) sending, 1 receiving) — endpoint exercised on one core
frame reclaim: post-teardown alloc 0x48440000 (exited client's root was 0x48440000)
frame reclaim: longest free run 32 MiB -> 32 MiB after teardown — every frame returned
  (3 task(s) still alive and holding their address space — send a newline to let the UART driver exit and the pool returns whole)
    task 6 (pid 6): blocked (waiting for a message), recv on ep22
    task 8 (pid 8): blocked (waiting for a message)
    task 11 (pid 11): blocked (waiting for a message)
shutting down (PSCI SYSTEM_OFF)
```

## Testing

Two layers, deliberately different in kind:

- **`cargo ktest-host`** — 221 tests over the portable crates (`abi`, `hal`,
  `cpio`, `fdt`, `framebuffer`, `videocore`, `virtio`, `iommu`, `mm`, `ipc`,
  `staros-libc`, `init`), including `fdt` against real `.dtb` blobs, `cpio` against a
  real archive, `hal`'s tick↔nanosecond arithmetic against the frequencies real
  machines report, and the C library's format engine, allocator and string
  functions. Fast, and they cover the code whose bugs are silent.
- **`./scripts/smoke-test.sh`** — builds one image and boots it across the machine
  matrix (GICv2 smp1, GICv2 smp4, GICv3 smp4, 128 MiB, `ramfb`, SMMU, and an
  8-core run without `--quick`), asserting on expected lines *and* the absence of
  failure signals. Currently **243 assertions, exit=0** on `--quick` (270 on the
  full matrix).
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

Two of the checks turned out to be measuring the host rather than the system, and both
are fixed: `input-check.sh` pressed its keys on a fifteen-second timer that the boot
outgrew, and now waits for the driver to say its queues are armed; `qml-verify` asked
whether an animated bar was wide in the one frame where an unrelated card was fullest,
and now asks every frame.

Design rationale, the SMP/IPC/IOMMU write-ups, and an honest "not yet
implemented" list live in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).
