# STAR OS Kernel — v0.2 (rewrite)

A `no_std` **microkernel for aarch64**, built as a clean multi-crate Cargo
workspace. This replaces the monolithic single-crate design in `../kernel-old`
(kept only as a cautionary reference — see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)).

Everything below runs today: EL2→EL1 entry, Linux/arm64 boot protocol, device
tree instead of hard-coded addresses, TTBR1 split with 4 KiB page tables, GICv2
**and** GICv3, PSCI + SMP, a preemptive scheduler, EL0 isolation, capabilities
with revocation, synchronous IPC, shared memory, an ELF loader, drivers and
interrupt handling in **user space**, an SMMUv3 enforced against a real bus
master, an initramfs, and a framebuffer console.

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
cargo ktest-host             # portable-crate unit tests on the host (82 tests)
./scripts/smoke-test.sh      # boot the whole matrix and assert on the output
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
smp: 4 core(s) online (PSCI v1.1)
smp: 4 cores x 20000 locked increments = 80000 (expected 80000) — no increments lost
[fault] task 3 killed: EL0 fault at 0x40000000 (ec 0x24) — isolated, kernel continues
[devicemgr] parsed the device tree in user space: PL011 @ 0x9000000 intid 33
[devicemgr] delegated UART device+irq to the driver and a device to the server
[child] hello - I was created at runtime, not by the kernel
```

## Testing

Two layers, deliberately different in kind:

- **`cargo ktest-host`** — 82 tests over the portable crates (`abi`, `cpio`,
  `fdt`, `framebuffer`, `videocore`, `iommu`, `mm`, `ipc`, `init`), including
  `fdt` against real `.dtb` blobs and `cpio` against a real archive. Fast, and
  they cover the code whose bugs are silent.
- **`./scripts/smoke-test.sh`** — builds one image and boots it across the machine
  matrix (GICv2 smp1, GICv2 smp4, GICv3 smp4, 128 MiB, `ramfb`, SMMU, and an
  8-core run without `--quick`), asserting on expected lines *and* the absence of
  failure signals. Currently **54 assertions, exit=0** on `--quick`.

## Status

Phases 1 and 2 of [`docs/ROADMAP-PIXEL.md`](docs/ROADMAP-PIXEL.md) are closed and
verified live in QEMU. Phase 3 is the watershed — first real hardware, a
**Raspberry Pi 5** — and its software groundwork is already in the tree (FDT,
framebuffer console, VideoCore mailbox, GICv2, PSCI/SMP). What remains there
needs the board, not more code.

Design rationale, the SMP/IPC/IOMMU write-ups, and an honest "not yet
implemented" list live in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).
