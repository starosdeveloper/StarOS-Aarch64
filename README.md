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
address space, processes loaded from a file rather than from the kernel image, and
a display server in user space that owns the screen.

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
| `crates/iommu` | SMMUv3 descriptor/queue bit layouts (no MMIO) |

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

Both EL0 programs are built by `crates/kernel/build.rs` and embedded in the
kernel image; there is no separate build step.

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
cargo krun                   # build + boot it in QEMU
cargo kclippy                # clippy across the workspace
cargo ktest-host             # portable-crate unit tests on the host (111 tests)
./scripts/smoke-test.sh      # boot the whole matrix and assert on the output
./scripts/fb-check.sh        # assert on the *pixels* the display server composited
./scripts/smoke-test.sh --quick   # same, minus the slow 8-core run
```

`cargo krun` boots via the `runner` in `.cargo/config.toml`, which flattens the
ELF to an `Image` and hands it to QEMU's arm64 Linux boot stub — **the same
protocol a real bootloader uses**, and the only path that passes a device tree in
`x0`. Exit QEMU with `Ctrl-A` then `X`.

Abridged output (`ramfb-el2-smp4`, one of the smoke-test configs):

```
STAR OS microkernel v0.2.0 — entered at EL2, running at EL1
device tree at 0x48000000 (1048576 bytes): linux,dummy-virt
  intc: GICv3, dist 0x8000000, redist 0x80a0000
  console: pl011 at 0x9000000 (+0x1000) — this console, found not assumed
MMU enabled: true (kernel in TTBR1 at 0xffff000000000000; ...)
framebuffer: ramfb 640x480 online (mirroring the console to the screen)
framebuffer: handed to displaysrv (id 14); the kernel logs to the UART from here
[displaysrv] the screen is mine: kernel output stopped, pixels are a process's now
[fbclient] 64x64 surface composited by displaysrv - 4096 pixels, and I never touched the screen
clock: 62500000 Hz counter, 16 ns per 1 tick(s) (exact)
clock: one tick interval (6250000 counter ticks) measured 101030 us against an expected 100000 us (agrees with the tick interval)
smp: 4 core(s) online (PSCI v1.1)
smp: 4 cores x 20000 locked increments = 80000 (expected 80000) — no increments lost
[fault] task 3 killed: EL0 fault at 0x40000000 (ec 0x24) — isolated, kernel continues
[devicemgr] parsed the device tree in user space: PL011 @ 0x9000000 intid 33
[devicemgr] delegated UART device+irq to the driver and a device to the server
[client] monotonic clock: two ClockNow reads from EL0, the second strictly later - no capability needed
[client] SleepUntil: woke no earlier than its 20 ms absolute deadline
[client] WaitAny: index 1 of 2 from the server's notification, a lone silent source timed out, ...
[client] SpawnThread: a thread in this very address space wrote through our page and ran with its own TPIDR_EL0
[devicemgr] started 'init.elf' from the initramfs as a new process - the kernel loaded a file, not a built-in image
sleep: 2 task-sleep(s) parked, 1 deadline(s) already past, 3 clock wake-up(s), worst overshoot 2889 us
[child] hello - I was created at runtime, not by the kernel
```

## Testing

Two layers, deliberately different in kind:

- **`cargo ktest-host`** — 113 tests over the portable crates (`abi`, `hal`,
  `cpio`, `fdt`, `framebuffer`, `videocore`, `iommu`, `mm`, `ipc`, `init`),
  including `fdt` against real `.dtb` blobs, `cpio` against a real archive, and
  `hal`'s tick↔nanosecond arithmetic against the frequencies real machines report.
  Fast, and they cover the code whose bugs are silent.
- **`./scripts/smoke-test.sh`** — builds one image and boots it across the machine
  matrix (GICv2 smp1, GICv2 smp4, GICv3 smp4, 128 MiB, `ramfb`, SMMU, and an
  8-core run without `--quick`), asserting on expected lines *and* the absence of
  failure signals. Currently **182 assertions, exit=0** on `--quick` (208 on the
  full matrix).
- **`./scripts/fb-check.sh`** — the only check that cares what the screen *looks*
  like: it boots with `ramfb`, screendumps over QMP, and asserts named coordinates
  (the client's surface where it asked for it, the server's background around it,
  no kernel console text left). A compositor that ignores its client's coordinates
  passes every text assertion above and fails this one.

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
