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
display server in user space that owns the screen, a virtio-input driver that
decodes real key events without the kernel seeing one, a file server that hands
files to processes holding no archive, and a **C program** — compiled by clang
against this tree's own libc — that prints, allocates, sleeps, reads files and runs
`pthread`s with real thread-local storage, without a syscall in sight.

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
| `crates/virtio` | Split-virtqueue layout, MMIO register map, input event format (no MMIO) |
| `crates/iommu` | SMMUv3 descriptor/queue bit layouts (no MMIO) |
| `crates/staros-libc` | The C library EL0 programs link against: `str*`/`mem*`/`printf`, `malloc` over `MapAnon`, time over `ClockNow`, files over the file server, and `pthread`s with real thread-local storage over `SpawnThread` |

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
| `services/displaysrv` | Display server: owns the framebuffer, composites client surfaces delivered as shared-memory capabilities |
| `services/inputsrv` | virtio-input driver: virtqueue, interrupt and event decoding, all in EL0 |
| `services/fssrv` | File server: owns the initramfs, answers `Open`/`Read`/`Stat`/`Close` over IPC through a client-supplied shared buffer |
| `services/fsclient` | A process with no archive and no device, reading a file anyway — the only way "these bytes arrived over IPC" means anything |
| `services/hello-c` | A program written in **C**, compiled by clang and linked against `crates/staros-libc` — the toolchain Qt will arrive through, exercised by something small enough to debug: formatting, the heap, the clock, files and four threads with their own TLS |

All seven EL0 programs are built by `crates/kernel/build.rs` and embedded in the
kernel image; there is no separate build step. The C one is skipped, with a
warning, on a host with no clang.

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
cargo ktest-host             # portable-crate unit tests on the host (147 tests)
./scripts/smoke-test.sh      # boot the whole matrix and assert on the output
./scripts/fb-check.sh        # assert on the *pixels* the display server composited
./scripts/input-check.sh     # press a key on the emulated keyboard and check the driver decoded it
./scripts/gdb-check.sh       # break inside an EL0 program over QEMU's gdbstub and unwind its stack
./scripts/libc-progress.sh   # score crates/staros-libc against the symbols Qt needs
./scripts/smoke-test.sh --quick   # same, minus the slow 8-core run
```

`cargo krun` boots via the `runner` in `.cargo/config.toml`, which flattens the
ELF to an `Image` and hands it to QEMU's arm64 Linux boot stub — **the same
protocol a real bootloader uses**, and the only path that passes a device tree in
`x0`. Exit QEMU with `Ctrl-A` then `X`.

The runner builds a *complete* machine on purpose: `-device ramfb` so there is a
screen to hand to `displaysrv`, `-device virtio-keyboard-device` so `inputsrv` has
a device to find, and an initramfs built beside the image (greeting, version, and
the `init` ELF) so the archive and `SpawnImage` paths run too. Anything left out
here is a subsystem that reports "this machine has none" and vanishes from the
log — which is how the display server and then the input driver each went
unnoticed after being written. The devices cost nothing when nothing uses them.

To press a key on that keyboard, run `./scripts/input-check.sh`: a headless
`cargo krun` has no way to deliver one, so the driver arms its queue and waits.

Abridged `cargo krun` output — everything below is from one run, with only the
device-tree detail lines and the font self-test elided:

```
STAR OS microkernel v0.2.0 — entered at EL1, running at EL1
device tree at 0x48200000 (1048576 bytes): linux,dummy-virt
  ram total: 256 MiB
  reserved regions: 0
exception vectors installed (VBAR_EL1)
privileged access never (PAN): not implemented on this CPU
MMU enabled: true (kernel in TTBR1 at 0xffff000000000000; RAM 0x40000000..0x50000000 Normal in the linear map; 44-bit PA per ID_AA64MMFR0_EL1)
memory: 256 MiB RAM, 126 MiB usable, heap 1280 KiB @ 0x401b5000, 125 MiB of frames @ 0x402f5000
kernel heap: Vec of 8 squares (last 64) sums to 204 — global allocator live
dynamic tables: 64 objects (old max 8), 64 caps in one task (old max 4), 64 notifications (old max 4) — all grew past the old fixed limits
framebuffer: ramfb 640x480 online (mirroring the console to the screen)
syscall Yield -> 0; syscall 0xdead -> -6
clock: 62500000 Hz counter, 16 ns per 1 tick(s) (exact)
interrupt controller: GICv2 online
clock: one tick interval (6250000 counter ticks) measured 101643 us against an expected 100000 us (agrees with the tick interval)
smp: 1 core(s) online (PSCI v1.1)
smp: 1 cores x 20000 locked increments = 20000 (expected 20000) — no increments lost
smp: single core — no inter-processor interrupt to send
loaded init ELF: 13240 bytes, entry 0x80000000
scheduler: capability delegation (client + server) + user-space IRQ driver
framebuffer: handed to displaysrv (id 14); the kernel logs to the UART from here
[fault] task 3 killed: EL0 fault at 0x40000000 (ec 0x24) — isolated, kernel continues
[fault]   backtrace (1 frames, x29 chain): 0x80000570
[devicemgr] parsed the device tree in user space: PL011 @ 0x9000000 intid 33
[devicemgr] delegated UART device+irq to the driver and a device to the server
[devicemgr] found a virtio-input device at a003e00 intid 79 and delegated it to the input driver
[devicemgr] no IOMMU on this machine; DMA capability stands but is unenforced
[devicemgr] unpacked the initramfs in user space: 3 files, no storage driver
[devicemgr] read 'greeting.txt' from the initramfs: hello from the initramfs
[displaysrv] the screen is mine: kernel output stopped, pixels are a process's now
[fssrv] the files are mine: 3 of them, served over IPC to processes that hold no archive
[fsclient] two endpoint capabilities and one page of my own memory - no archive, no device
[fssrv] the files are mine: 3 of them, served over IPC to processes that hold no archive
[hello-c] a C program in EL0: printf, malloc, clock and files, no syscall in sight
[hello-c] heap: 103 allocations, 368 bytes live at the end
[stack] walked 40 pages down a stack that started with one mapped, every marker read back - pages arrived on demand
[fault] task 18 killed: EL0 fault at 0x7fffaffb0 (ec 0x24) — stack guard: growth limit reached — isolated, kernel continues
[fault]   backtrace (2 frames, x29 chain): 0x8000080c 0x80000804
[child] hello - I was created at runtime, not by the kernel
[loaded] hello - my ELF was a file in the initramfs, parsed in user space and handed to the kernel as bytes
[client] monotonic clock: two ClockNow reads from EL0, the second strictly later - no capability needed
[driver] user-space UART-RX driver waiting for input
[driver] newline received; user-space IRQ driver exiting
[inputsrv] virtio-input driver up in EL0: queue armed, waiting for the device
[devicemgr] started 'init.elf' from the initramfs as a new process - the kernel loaded a file, not a built-in image
[devicemgr] the kernel refused a non-ELF file and an unmapped pointer, as it must
[displaysrv] composited a client surface onto a screen the client cannot touch
[fbclient] 64x64 surface composited by displaysrv - 4096 pixels, and I never touched the screen
[fsclient] stat 'greeting.txt' over IPC: 25 bytes, mode 100644
[hello-c] clock: 133086384 ns across a 20 ms nanosleep
[client] SleepUntil: woke no earlier than its 20 ms absolute deadline
#server drove the UART, then revoked it for everyone
[child] hello - I was created at runtime, not by the kernel
[client] read from shared memory: shared-memory works: written by the server, read by the client
[client] read the marker from the SECOND page of a 2-page shared buffer
[fsclient] read 'greeting.txt' through fssrv in 2 chunks: hello from the initramfs
[fsclient] 25 of 25 bytes in 2 reads, the second one from offset 6
[client] WaitAny: index 1 of 2 from the server's notification, a lone silent source timed out, a later signal was still counted, and no stale registration poisoned the next block
[child] hello - I was created at runtime, not by the kernel
[client] SpawnThread: a thread in this very address space wrote through our page and ran with its own TPIDR_EL0
[cap] task 0 denied MapMemory(handle 0): no such capability
[client] kernel refused a syscall pointer into an unmapped page - it walks our tables, not a range
[parent] spawned 3 children via the Spawn syscall - 15 tasks total, old table held 8
[fsclient] fssrv refused an unopened handle, a missing file, a closed handle and a lied-about length
[fsclient] asked for all 13240 bytes of 'init.elf' into a 4096-byte buffer and got 4096, with 9144 left
[hello-c] read 'greeting.txt' through fssrv with libc's open/read/lseek: hello from the initramfs
[fsclient] the archive is at 0x900000000 in fssrv; touching it here must fault
[fault] task 12 killed: EL0 fault at 0x900000000 (ec 0x24) — isolated, kernel continues
[fault]   backtrace (3 frames, x29 chain): 0x80000014 0x800016c8 0x80000004
[fssrv] served 12 requests, 4121 bytes of file data, and refused 4 - the archive never left this address space
[ipc-storm] receiver drained every message from 3 concurrent senders, sequence sum exact - no message lost or duplicated
[memtest] MapAnon(0) refused - a zero-page request is an error, not a page
[memtest] DMA buffer: 4 physically-contiguous non-cacheable pages, first and last written and read back
[memtest] 2.5 MiB .bss reaches 2.25 MiB in (past the 2 MiB L2 boundary); grew the heap by 16 MiB in 8 calls of 1024 pages, first and last page of every run zeroed then written and read back, runs handed out back to back
[hello-c] threads: 4 workers x 250 increments = 1000, 1 thread(s) live at the end
[hello-c] C RUNTIME OK - every check passed
[fssrv] served 7 requests, 44 bytes of file data, and refused 1 - the archive never left this address space
clock: the demo took 13520 ms on the monotonic clock, during which core 0 took 106 tick(s)
sleep: 4 task-sleep(s) parked, 1 deadline(s) already past (returned at once), 5 clock wake-up(s), worst overshoot 16043 us
scheduler: all tasks finished after 106 timer ticks; task table grew to 28 (old fixed max 8)
task teardown: reaped 27 dead-task kernel stacks (864 KiB returned to the heap)
user stacks: 40 page(s) mapped on demand (160 KiB), 1 mapped up front per task, limit 256 KiB
preemption: timer ticks per core — cpu0=106
ipc storm: 192 sends / 192 recvs on one endpoint — cpu0=192s/192r (1 core(s) sending, 1 receiving) — endpoint exercised on one core
frame reclaim: post-teardown alloc 0x40307000 (exited client's root was 0x402f8000)
frame reclaim: longest free run 32 MiB -> 32 MiB after teardown — every frame returned
  (1 task(s) still alive and holding their address space — send a newline to let the UART driver exit and the pool returns whole)
shutting down (PSCI SYSTEM_OFF)
```

## Testing

Two layers, deliberately different in kind:

- **`cargo ktest-host`** — 147 tests over the portable crates (`abi`, `hal`,
  `cpio`, `fdt`, `framebuffer`, `videocore`, `virtio`, `iommu`, `mm`, `ipc`,
  `staros-libc`, `init`), including `fdt` against real `.dtb` blobs, `cpio` against a
  real archive, `hal`'s tick↔nanosecond arithmetic against the frequencies real
  machines report, and the C library's format engine, allocator and string
  functions. Fast, and they cover the code whose bugs are silent.
- **`./scripts/smoke-test.sh`** — builds one image and boots it across the machine
  matrix (GICv2 smp1, GICv2 smp4, GICv3 smp4, 128 MiB, `ramfb`, SMMU, and an
  8-core run without `--quick`), asserting on expected lines *and* the absence of
  failure signals. Currently **202 assertions, exit=0** on `--quick` (229 on the
  full matrix).
- **`./scripts/input-check.sh`** — the only check that makes the *outside world*
  act: QEMU synthesises a real key event, and the assertion is that a driver in EL0
  decoded it, with no kernel code anywhere in the path.
- **`./scripts/fb-check.sh`** — the only check that cares what the screen *looks*
  like: it boots with `ramfb`, screendumps over QMP, and asserts named coordinates
  (the client's surface where it asked for it, the server's background around it,
  no kernel console text left). A compositor that ignores its client's coordinates
  passes every text assertion above and fails this one.
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
microkernel, which of those pieces already exist here, and the ABI gaps (monotonic
time, multiplexed waiting, threads inside one address space) that block a Qt event
loop long before any Qt code enters the tree.

Design rationale, the SMP/IPC/IOMMU write-ups, and an honest "not yet
implemented" list live in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).
