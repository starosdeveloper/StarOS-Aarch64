# STAR OS Kernel — Architecture (v0.2)

This is a ground-up rewrite of the STAR OS kernel. The `../kernel-old` tree is
kept only as a reference for **what not to do**.

## What went wrong in the old design

The previous kernel was a single `staros-kernel` crate containing ~54,000 lines
across ~196 files: memory, drivers (1.1 MB of source), the network stack,
crypto, the display server, FFI — everything in one `crate::` namespace.

Concrete consequences we are designing away from:

- **No boundaries.** Any module could reach into any other. There was no way to
  reason about, or test, a subsystem in isolation.
- **`cfg` sprawl.** `std`/`no_std`/`test` were multiplexed inside one crate with
  `#[cfg(not(test))]` on whole modules, so the thing you tested was not the
  thing you shipped.
- **It didn't build.** The old README itself reports 92 compilation errors, 371
  `unsafe` blocks with ~25% documented, and 269 `panic!`/`unwrap()` sites.
- **Monolithic, not micro.** Despite the "microkernel" label, drivers and the
  network stack were compiled into the privileged core.

## Principles for v0.2

1. **Many small crates, explicit edges.** Each subsystem is its own crate with a
   declared dependency list. The dependency graph *is* the architecture.
2. **Portable core, contained `unsafe`.** Pure logic (addresses, allocators, IPC
   message types) is `no_std` but host-testable. Raw MMIO and assembly live only
   in the arch crate.
3. **Program against traits.** The kernel core depends on `staros-hal` traits,
   not on concrete chips. Swapping hardware means adding a crate, not editing the
   scheduler.
4. **Truly micro.** Drivers and services are out-of-kernel by default and talk
   over IPC. The `kernel` binary stays thin.
5. **The compiler enforces hygiene.** Workspace lints require documented
   `unsafe`, safety docs, and `unsafe_op_in_unsafe_fn`.

## Crate map

```
kernel-new/
├── crates/
│   ├── abi/            # syscall numbers, error codes, Handle — the kernel↔user contract
│   ├── hal/            # hardware traits: SerialConsole, Timer, InterruptController
│   ├── fdt/            # device tree parser — zero-alloc, no deps, host-tested
│   ├── cpio/           # initramfs (newc CPIO) reader — zero-alloc, host-tested
│   ├── mm/             # PhysAddr/VirtAddr, FrameAllocator, buddy + free-list heap (host-tested)
│   ├── ipc/            # Message, Endpoint — fixed-size, allocation-free
│   ├── iommu/          # SMMUv3 STE / stage-2 / command bit-layouts — pure, host-tested
│   ├── framebuffer/    # 8x8-font text console over a linear framebuffer — host-tested
│   ├── videocore/      # Raspberry Pi VideoCore mailbox property messages — host-tested
│   ├── arch-aarch64/   # boot stub, MMU/TTBR1, exceptions+PAN, GICv2/v3, timer, PSCI+SMP,
│   │                   #   context switch, PL011, SMMUv3 driver, PCI ECAM, ramfb/mailbox, usercopy
│   ├── drivers/        # Driver lifecycle trait (framework only)
│   └── kernel/         # the privileged binary; wires the above together
├── services/
│   ├── init/           # first user-space process: host-testable protocol lib +
│   │                   #   boot/image.rs, the loadable EL0 binary (built by build.rs)
│   └── devicemgr/      # the bootstrap EL0 process (parses DTB, unpacks initramfs, delegates)
└── docs/
```

### Dependency direction

```
abi  ◄─────────────┬──────────┬──────────┐
                   │          │          │
hal ◄── arch-aarch64        drivers      │
 ▲          ▲                  ▲         │
 │          │                  │         │
 └───────── kernel ────────────┘         │
                │                        │
              mm, ipc ────────────────────┘
```

`abi` depends on nothing. Nothing depends on `kernel`. Arch-specific `unsafe`
never leaks upward past the HAL traits.

## Boot flow

1. A bootloader loads the flat `Image` per the ARM64 Linux boot protocol: it
   reads the 64-byte header at `_start`, places the image at `text_offset` above
   the 2 MiB-aligned base of DRAM, and jumps to it with **`x0` = physical address
   of the device tree**, at **EL2 if the hardware has it** (Linux asks for EL2 so
   KVM can use it). `scripts/qemu-run.sh` flattens the ELF and boots it this way,
   so the everyday path is the same protocol a real device uses.
2. The stub (`arch-aarch64/src/boot.rs`) parks secondary cores and, if it finds
   itself at EL2, **drops to EL1** — this is an EL1 kernel, and left upstairs its
   `TTBR*_EL1` and `SCTLR_EL1` would govern a regime it is not running in, so the
   MMU would never come on and the branch to its link address would land in
   unmapped space with no console to report it. It then builds the translation
   tables, turns the MMU on, jumps to its high link address, sets
   `sp = __stack_top`, enables FP/SIMD access (`CPACR_EL1.FPEN`, so
   compiler-emitted NEON does not trap), and calls `rust_start` with `x0` = the
   device tree pointer and `x1` = the level it was entered at.
3. `rust_start` records the entry level (`boot::entry_el`, the one fact about the
   handoff that cannot be recovered later — `CurrentEL` reads 1 either way once
   the stub is done) and calls `kmain(dtb)` (defined in the `kernel` crate,
   resolved at link time via `extern "Rust"`).
4. `kmain` parses the device tree *before touching any hardware* — the parser
   needs no console, heap or MMU, which is what lets everything after it be told
   where the hardware is — brings the console up at the address the tree reports,
   then installs vectors, enables the MMU, and starts the scheduler.

## Roadmap

Done:

- **EL1 exception vectors** (`arch-aarch64/src/exceptions.rs`). A 16-entry
  `VBAR_EL1` table, a shared save/restore trampoline that builds a `TrapFrame`,
  and a Rust handler that decodes `ESR_EL1`. Verified in QEMU: a demo `svc #0`
  traps (kind 4, EC 0x15), is handled, and `eret`s back into `kmain`.

- **Syscall dispatch on the SVC path.** The arch handler decodes the calling
  convention (number in `x8`, args in `x0`..=`x5`) into a `SyscallRequest` and
  delegates to `kernel::syscall::staros_syscall_dispatch` (bound by link name,
  like `kmain`) — so syscall *policy* stays in the kernel, not the arch crate.
  The result is written back to `x0`. Verified in QEMU: `Yield -> 0`, an
  out-of-range number -> `-6` (`NoSuchSyscall`).

- **IRQ path: GICv2 + generic timer.** `arch-aarch64::gic` implements
  `hal::InterruptController` over the GICv2 distributor/CPU interface;
  `arch-aarch64::timer` drives the EL1 physical timer (`CNTP`, INTID 30). The
  IRQ vector (kind 5) acknowledges at the GIC and delegates the interrupt id to
  `kernel::irq::staros_irq_dispatch` (same link-time seam as syscalls), which
  re-arms the timer and counts ticks. Verified in QEMU: timer ticks fire at
  10 Hz through trap → ack → dispatch → EOI.

- **Preemptive round-robin scheduler + first context switch.**
  `arch-aarch64::context` provides the switch primitive: a `CpuContext`
  (callee-saved `x19`..=`x30` + `sp`), `context_switch`, and a trampoline that
  bootstraps new tasks (enables IRQs, calls the entry, exits on return).
  `kernel::sched` is the policy: fixed task slots with per-task stacks, states,
  round-robin `pick_next`, and cooperative `yield_now` / `exit`. The timer tick
  requests a reschedule that runs in the IRQ epilogue *after EOI* (so the switched
  in task can receive its own timer interrupts). `Syscall::Yield` now reschedules.
  Verified in QEMU: two tasks interleave under 10 Hz preemption (A/B/A/B…), each
  runs three rounds of busy work longer than the tick period, both exit, and
  control returns to the bootstrap thread.

  Borrowing note: scheduler state lives in one `UnsafeCell` static; no reference
  to it is ever held across a `context_switch` (that would let the switched-in
  task alias it) — each op extracts raw `CpuContext` pointers under a short borrow,
  then switches.

- **MMU bring-up.** `arch-aarch64::mmu` builds a two-level table (4 KiB granule,
  1 GiB level-1 blocks) and enables translation plus caches: Normal write-back
  over RAM (kernel image, stacks, page tables), Device memory below it (UART/GIC
  MMIO). Originally this was an identity map over `TTBR0_EL1`, built by Rust after
  boot; it is now the kernel's linear map in `TTBR1_EL1`, built in assembly before
  Rust runs at all (see the TTBR1 section below). Verified in QEMU: after `MMU
  enabled: true`, the full syscall + scheduler flow keeps running — i.e. both the
  Device and Normal mappings are correct under live translation.

- **EL0 user mode + memory isolation.** The EL0 window is mapped down to 4 KiB
  pages: code pages are EL0 read+execute, stacks EL0 read/write, and everything
  else is unreachable from EL0. `arch-aarch64::usermode::enter_el0` stages
  `ELR/SP_EL0/SPSR`
  and `eret`s to EL0. Verified in QEMU with a naked user program that (1) prints
  via the `DebugPutc` syscall — proving EL0 execution and the EL0→EL1 trap
  (vector kind 8, EC 0x15) — and (2) reads a kernel address and takes a data
  abort (EC 0x24, `FAR` = the kernel address), which the handler reports as an
  isolation fault. User space cannot touch kernel memory.

- **Per-task address spaces (distinct `TTBR0`) from `mm::FrameAllocator`.**
  `arch-aarch64::addrspace::AddressSpace` builds a private page-table tree per
  process out of frames from a `FrameAllocator` (over the RAM the device tree
  reported). A space holds a per-process EL0 window at `0x8000_0000` — the loaded
  image plus *private* data and stack frames — and nothing else: the EL1 handler
  keeps working while a user `TTBR0` is active because the kernel is in `TTBR1`,
  not because each space carries a copy of its map. The scheduler (`kernel::sched`) now tags each task with a `TTBR0`
  and installs it (`mmu::set_ttbr0`, with a TLB flush) before switching in; user
  tasks are ordinary scheduler tasks whose entry drops to EL0, and when preempted
  at EL0 their full state rides in the trap frame on their kernel stack, which the
  callee-saved switch preserves. `Syscall::Exit` now ends the *task* (not the
  core). Verified in QEMU: two kernel threads and two EL0 processes run under one
  preemptive scheduler; both processes read the *same* virtual address
  (`USER_DATA_VA`) yet print *different* ids (`#1` vs `#2`) — same VA, different
  physical frame — while the timer keeps ticking (6 ticks), then all four tasks
  exit and control returns to the bootstrap thread.

- **Synchronous IPC endpoints (`Send`/`Recv`) wired to the scheduler.**
  `kernel::ipc` gives each endpoint a bounded ring of pending `staros_ipc::Message`s
  and a FIFO of blocked receivers. `recv` returns a buffered message or parks the
  caller (`sched::block_for_message` marks it `Blocked` and switches away); `send`
  hands the message straight to a waiting receiver (`sched::deliver` wakes it) or
  buffers it, and never blocks the sender. The `Send`/`Recv` syscalls are thin
  adapters over these. Verified in QEMU: two EL0 processes that share *no* memory
  act as sender and receiver on endpoint 0 — process #1 sends `A`/`B`/`C`,
  process #2 receives `A`/`B`/`C` in order, every byte crossing through the
  kernel — while the two kernel threads keep running under preemption.

- **A user-space device driver (`MapMemory` + EL0 MMIO + IPC).** `MapMemory`
  (`x0` = device physical base) maps an MMIO page into the *caller's* address
  space and returns its user VA: `arch::addrspace::AddressSpace` gained a `Copy`
  handle and `map_device`, which walks to the leaf table for `USER_DEV_VA`, writes
  one Device/EL0-RW descriptor there and flushes the
  TLB; the scheduler tags each user task with its `AddressSpace` so
  `sched::map_device_current` can map into whoever called. Policy stays in the
  syscall layer — only the PL011 page is mappable for now. Verified in QEMU: EL0
  process #2 is a console *driver* that maps the UART itself and writes bytes
  straight to its data register (its own MMIO, no kernel `DebugPutc`); EL0
  process #1 (no device access, disjoint address space) streams a string to it
  over endpoint 0. The line `hello from a user-space driver via IPC` reaches the
  screen produced entirely by the unprivileged driver — the kernel does none of
  that I/O. This is the microkernel thesis running: a driver out of the kernel,
  talking to its client only through IPC.

- **Capabilities: per-task authority, checked on every syscall.** `kernel::cap`
  gives each task a small capability table; user space names objects only by
  `Handle` (an index into *its own* table), never by raw id or address. A `Cap`
  is either an `Endpoint { id, send, recv }` or a `Device { phys }`. `Send`/`Recv`
  now take an endpoint *handle* and require the matching direction right;
  `MapMemory` takes a *device-capability* handle and maps exactly the page it
  authorises — the earlier hard-coded endpoint id and "only PL011" allow-list are
  gone, replaced by rights granted at spawn (`sched::spawn_user` takes a
  `CapTable`; `sched::resolve_cap` resolves a handle against the caller). Verified
  in QEMU: the client, granted only *send* on endpoint 0, is **denied** when it
  tries to map the UART (`[cap] task 2 denied MapMemory(handle 1): handle is not a
  device capability`), while the driver, holding the device capability on handle
  2, maps it and drives the console. Same syscall, opposite outcome — decided
  solely by the capability the task holds. Unforgeable: handle 1 means different
  authority in each task's table.

- **Rich IPC and dynamic capabilities: full messages, capability transfer,
  blocking send, revocation.** A `Message` (`repr(C)`: tag + inline words + a
  `Handle`) is now delivered end-to-end via a *user buffer* — `Send`/`Recv` take
  an endpoint handle and a pointer to the caller's `Message`, and the kernel
  copies it through (`user_ptr_ok` bounds-checks the pointer against the EL0
  window; cortex-a72 has no PAN, so EL1 may touch the mapped EL0 page). Endpoints
  gained a blocked-*sender* queue as well as a blocked-receiver one, so `Send`
  **blocks** when the ring is full and a draining receiver wakes it. A message can
  **transfer a capability**: the sender names one in `Message.cap`, the kernel
  resolves it from the sender's table into a kernel `Cap`, and on `Recv` installs
  it into the receiver's table under a fresh handle (`sched::install_cap_current`)
  — dynamic, per-task capability allocation.

- **Generational object table: cross-task revocation.** Capabilities no longer
  point at objects directly; a `Cap` carries rights plus an `ObjectRef {index,
  generation}` into a global object table (`kernel/src/obj.rs`). Every syscall
  resolves the handle to a `Cap`, then the `ObjectRef` to a *live* object via
  `obj::get`, which returns `None` if the slot's generation has moved on. `Revoke`
  bumps that generation and clears the slot, so **every** outstanding capability to
  the object — in any task, including copies delegated over IPC — stops resolving
  at once. Verified in QEMU: a resource server delegates a copy of its UART
  capability to a client, drives the UART itself, then `Revoke`s the UART *object*
  and exits; when the client (holding a valid handle) tries to map it, the kernel
  denies it — `denied MapMemory(handle 3): object was revoked`.

- **Reclaiming buddy allocator + `AddressSpace` teardown.**
  `mm::BuddyFrameAllocator` manages the frame pool as a binary buddy tree
  (longest-free-run per subtree), so freed frames coalesce and are handed back out.
  Each `AddressSpace` records the frames it privately owns (tables + data/stack +
  loaded segment pages); `AddressSpace::destroy` returns them all to the allocator,
  and `sched::exit` calls it once the dead task's `TTBR0` is off the CPU. Verified
  in QEMU: `frame reclaim: post-teardown alloc 0x40400000 (exited client's root was
  0x40400000)` — an exited task's root frame is reclaimed and reused.

- **`init` loaded from a real ELF by an in-kernel ELF loader.** The first EL0
  program is no longer assembly baked into the kernel's `.text`, nor a flat blob.
  It lives in `services/init/boot/image.rs`, a standalone `no_std`/`no_main`
  program linked at `USER_BASE` by `boot/image.ld` into two `PT_LOAD` segments
  (read-execute `.text`/`.rodata`, read-write `.data`/`.bss`); the kernel's
  `build.rs` compiles it with a plain `rustc` (the target's pre-compiled `core`
  suffices for a dependency-free naked `_start`, so no `build-std` or nested Cargo)
  and keeps the ELF. The kernel `include_bytes!`s it and `kernel/src/elf.rs` parses
  the ELF64 header and program headers; `AddressSpace::map_segment` backs each
  segment with fresh frames, copies its file image, zero-fills the `.bss` tail, and
  maps every page with the rights `p_flags` requests (W^X: R-X text, R-W data,
  R-only otherwise), running `cache::sync_instruction` over executable pages. The
  task enters at the ELF's `e_entry`. Verified in QEMU: `loaded init ELF: 9080
  bytes, entry 0x80000000`, and the server prints a `#` byte round-tripped through
  its `.data`/`.bss` — proof the writable segment loaded correctly — ahead of its
  greeting. The `services/init` *library* remains the host-testable home of the
  shared protocol logic.

- **Interrupts delivered to a user-space driver (notifications + IRQ caps).** The
  last half of "drivers live outside the kernel": not just their MMIO but their
  *interrupts* are handled at EL0. `kernel/src/notify.rs` adds an asynchronous
  **notification** object — the signal-only counterpart to an endpoint, with a
  pending count so no edge is lost. A driver holds an `Interrupt` capability
  (`Cap::Irq`, naming a GIC `intid`) and calls `IrqRegister`, which mints a
  notification, binds the line to it (`kernel/src/irq.rs`), enables it at the GIC,
  and hands back a notification handle. When the line fires, `staros_irq_dispatch`
  **masks it at the controller** (so a level-triggered source cannot storm),
  `signal`s the notification, and asks for a reschedule; the driver, blocked in the
  `Wait` syscall, wakes, services the device through its *own* MMIO mapping, and
  calls `IrqAck` to re-enable the line. Verified in QEMU: a third EL0 process is a
  UART-RX driver that maps the PL011, unmasks `IMSC.RXIM`, registers `intid 33`,
  and echoes typed input — `printf 'ping\n' | qemu …` prints `[driver] … waiting
  for input`, then `ping`, then exits on newline, all driven from EL0 with the
  kernel only forwarding the interrupt.

- **EL0 fault isolation.** A fault taken from a user task no longer halts the
  kernel. `handle_sync` distinguishes a synchronous exception from a lower EL
  (vector kinds 8..=11) that is *not* an `SVC` — a faulting user task — from a
  kernel fault: the former calls the kernel hook `staros_user_fault` (the same
  link-time seam as `kmain`/syscalls), which reports the fault and terminates just
  that task via `sched::exit`, scheduling the next runnable one; only a kernel
  fault is still fatal. This is what makes a user-space driver crash *contained* —
  the whole point of pushing drivers to EL0. Verified in QEMU: a fourth EL0
  process holding no capabilities reads kernel-only memory and the kernel prints
  `[fault] task 3 killed: EL0 fault at 0x40000000 (ec 0x24) — isolated, kernel
  continues`, after which the other tasks run to completion normally.

- **Kernel heap (`alloc`).** `mm::heap::FreeListAllocator` is a portable,
  host-tested address-ordered free list (holes carry their header in-place;
  adjacent frees coalesce). `kernel/src/heap.rs` owns a static region and wires it
  to Rust's `alloc` as the `#[global_allocator]`, masking IRQs around each
  operation (an interrupt handler may allocate). With it the kernel can use `Box`
  and `Vec` instead of fixed-size static arrays. Verified in QEMU: a heap-backed
  `Vec` of eight squares prints `… sums to 204 — global allocator live`.

- **Anonymous user memory (`MapAnon`).** A task grows its own address space at
  runtime: the `MapAnon` syscall maps one fresh zero page (from the buddy pool)
  read/write at the next free VA in a per-space heap region above the fixed
  data/stack/device pages, and returns it — no capability required, since a task
  may always allocate its own memory. `AddressSpace` tracks a `heap_next` cursor
  and adds the page to its owned set so teardown reclaims it. Verified in QEMU: a
  capability-less process maps two pages, writes and reads back `0xDEADBEEF`, and
  reports `[memtest] MapAnon gave writable pages; readback ok`.

- **User-space process creation (`Spawn`).** The kernel no longer hard-codes every
  task: the `Spawn` syscall builds a fresh EL0 process from the init image, seeded
  with a caller-chosen id, and schedules it — user space grows its own process
  tree. Verified in QEMU: a "spawner" process calls `Spawn`, and a child that the
  kernel never set up at boot runs and prints `[child] hello — I was created at
  runtime, not by the kernel`.

- **The ARM64 Linux boot protocol + a device tree parser: the kernel is told what
  machine it is on.** Everything above was written against constants — `virt`'s
  UART at `0x0900_0000`, a GICv2, a 4 MiB frame pool carved out by the linker.
  Real hardware hands the kernel exactly one thing: a pointer in `x0` to a
  flattened device tree. Two pieces close that gap. First, `arch-aarch64::boot`
  now begins with the 64-byte ARM64 Linux image header (`text_offset`,
  `image_size` from the linker, magic `ARM\x64`) and the build flattens the ELF
  to an `Image`, so a *bootloader* — QEMU's boot stub today, an Android ABL on a
  device — can load it; the stub is written to preserve `x0`, which flows through
  `rust_start` into `kmain(dtb)`. (This mattered immediately: booted as an ELF,
  `x0` is zero — QEMU's ELF path passes no tree, so the protocol is not optional
  for testing any of this.) Second, `crates/fdt` parses the blob: zero
  allocation, no dependencies, no panics on malformed input (firmware is outside
  the trust boundary — a null or misaligned `x0` is rejected without being
  dereferenced), host-tested against real DTBs dumped from QEMU. It resolves
  paths (`/memory` matches `memory@40000000` — unit addresses are board-specific),
  binds by `compatible` rather than address, and decodes `reg` with the cell
  counts the parent declared. `kmain` now parses the tree *before touching any
  hardware* and brings the console up at the address the machine reported.
  Verified in QEMU: **one kernel image, unchanged (same md5), across three
  machines** — `-m 256M`/`2G`/`1G`, `-smp 1`/`4`/`2`, `gic-version=2`/`3` — and
  every reported number tracks: `ram total: 2048 MiB`, `cpus: 4`,
  `intc: GICv3 at 0x8000000 — NOT YET SUPPORTED`, `console: pl011 at 0x9000000 —
  this console, found not assumed`. The full seven-process demo still runs
  end-to-end over the bootloader path.

- **GICv3, selected at runtime — the HAL boundary tested for real.** Every Pixel
  is GICv3, and it is not a revision of GICv2 so much as a different design: the
  CPU interface stops being MMIO and becomes system registers (`ICC_*_EL1` — an
  acknowledge is now an `mrs`, not a load), each core gets a *redistributor*
  frame that must be woken out of low-power before it delivers anything, SGIs and
  PPIs (id < 32) are configured there rather than in the distributor, SPIs must
  additionally be *routed* to a specific core via `GICD_IROUTER` (which requires
  affinity routing, `GICD_CTLR.ARE`), and enable/disable became asynchronous, so
  writes must be followed to completion through `RWP` — otherwise `kernel::irq`'s
  mask-on-fire would return while the line was still live, which is exactly the
  storm it exists to prevent. `arch-aarch64::gic` now implements both behind one
  runtime-selected `Gic` enum: the kernel reads `compatible` off the tree,
  decodes the two `reg` ranges (which mean *different things* per architecture —
  distributor + redistributors on v3, distributor + CPU interface on v2), and
  hands the arch layer a `GicSpec`; `find_redistributor` matches this core's
  `MPIDR_EL1` affinity against `GICR_TYPER` rather than assuming frame 0, so
  secondary cores will each find their own.

  **The result is the argument for the whole design.** Two controllers this
  different, and `kernel::irq` — dispatch, the notification bindings, the
  level-triggered mask discipline, the user-space driver protocol — changed by
  exactly nothing: `GIC.enable(x)` became `controller().enable(x)`, a name, not a
  line of logic. Verified in QEMU on one image (same md5): `gic-version=2` and
  `gic-version=3` both boot the full seven-process demo, with the EL0 UART-RX
  driver taking its interrupt (SPI 33, routed via `IROUTER` on v3) and the timer
  (PPI 30, enabled in the redistributor on v3) driving preemption. `interrupt
  controller: GICv3 online` … `ping` … `[driver] newline received`.

- **The memory map is now the machine's, not the linker's.** The previous entry
  left the kernel knowing how much RAM it had and still allocating out of a fixed
  4 MiB pool the linker script reserved. That pool is gone, and with it the last
  place where a constant stood in for the hardware. `kmain` surveys the tree —
  RAM banks for the extent, then everything that must not be handed out: the
  kernel image (`__image_start`..`__image_end`, which is why the linker exports
  them), the DTB blob itself (the kernel is standing on it while parsing it), the
  initrd, the `mem_rsvmap` header block *and* `/reserved-memory` nodes (on `virt`
  these are empty; on a phone they are dozens of carveouts, and writing into one
  is a silent reset, so both forms are honoured) — and `mm::region::largest_free`
  sweeps the exclusions to pick the biggest surviving run. That run is split:
  the kernel heap is carved off the front, sized as
  `BuddyFrameAllocator::metadata_bytes(rest) + 1 MiB` slack, and the remainder
  becomes the frame pool. The ordering is not circular — the heap gets raw bytes
  from the map, and only the buddy tree's `Box<[u32]>` metadata comes from the
  heap — which is what let the buddy allocator lose its `BUDDY_MAX_FRAMES` cap
  and size itself to the machine. `mmu::init` likewise takes the RAM window and
  writes the linear map from it (normal memory over RAM, device below it,
  *unmapped* above, so a stray access faults instead of wandering).

  Verified in QEMU across four machines on one unchanged image, and the numbers
  are the proof: `-m 2G` → `2048 MiB RAM, 1919 MiB usable, heap 3072 KiB @
  0x48100000, 1024 MiB of frames`; `-m 256M` → `heap 1152 KiB, 64 MiB of frames`;
  `-m 128M` → `128 MiB RAM, 63 MiB usable, heap 1088 KiB @ 0x44100000, 32 MiB of
  frames @ 0x44210000`. The heap lands *immediately after the DTB* in each case
  (QEMU places the blob at `ram_base + min(ram/2, 128 MiB)`, so it moves with
  `-m`, and the heap follows it) — the exclusion logic tracking a moving blob
  across configurations is the thing that could not be faked. The full
  seven-process demo, IPC, the EL0 fault, and frame reclaim all still run.

  One deliberate limitation remains: **the buddy tree rounds the pool down to a
  power of two**, so 1919 MiB usable becomes 1024 MiB managed and the tail is
  unowned. The boot line prints both numbers so the gap is visible rather than
  quietly lost. Fixing it means managing a region as several power-of-two arenas.
  (The other limitation — a gigabyte of RAM excluded because the kernel shared
  `TTBR0` with user space — is gone; see below.)

- **Entered at EL2, runs at EL1.** A real bootloader hands off at EL2, because
  Linux asks for it so KVM can use it. This kernel is an EL1 kernel — its vectors
  are `VBAR_EL1`, its translation `TTBR*_EL1`, its timer the EL1 physical timer —
  so the stub reads `CurrentEL` and, at EL2, sets `HCR_EL2.RW` (which also clears
  `E2H`/`TGE`: an ordinary EL1 kernel under a dormant EL2, not a VHE host with
  EL1 registers redirected under it), grants EL1 the physical timer via
  `CNTHCTL_EL2.EL1PCTEN|EL1PCEN` (not a formality — `timer.rs` uses `CNTP_*`, and
  without it the first timer access traps to EL2 instead of arming preemption),
  zeroes `CNTVOFF_EL2` and `VTTBR_EL2`, clears `CPTR_EL2.TFP`, gives `SCTLR_EL1` a
  defined value (its reset value is architecturally UNKNOWN when entered at EL2),
  and `eret`s to the next instruction at EL1h.

  Left undone this was not a missing feature but a silent death: with
  `-M virt,virtualization=on` the kernel printed *nothing at all*, because
  `SCTLR_EL1.M` governs a regime EL2 is not executing in, so translation never
  came on and the branch to the high link address landed in unmapped space before
  there was a console to complain with. The banner now reports `entered at EL2,
  running at EL1` versus `entered at EL1, running at EL1` — the stub carries the
  level to `rust_start`, since afterwards `CurrentEL` reads 1 either way and the
  two boots are indistinguishable. Verified on one unchanged image across
  `-m 128M`/`-m 2G` × GICv2/GICv3 × virtualisation on/off. An EL3 handoff is still
  not handled — the kernel says so and halts rather than pretending; Android
  bootloaders do not do that.

- **The kernel lives in `TTBR1`; user space owns `TTBR0` outright.** The kernel
  is linked at `KERNEL_VA_OFFSET + physical` (`0xFFFF_0000_...`) and loaded low,
  the linker script stating the split with `AT(ADDR(sec) - KERNEL_VA_OFFSET)` so
  the raw image is still laid out by physical address and the ARM64 Linux `Image`
  path keeps working. `phys_to_virt` is an OR and `virt_to_phys` an AND, not a
  lookup — which is what lets code that is about to *build* a page table use them.

  A bootloader enters with the MMU off, so the PC and every formable address is
  physical and every high address in the image is unusable: the boot stub must
  therefore build the tables in assembly, out of `adrp/add` (PC-relative → a
  symbol's physical address) and `ldr xN, =symbol` (link-time high address → the
  jump upstairs). It cannot read the device tree first, because the tree is only
  readable *through* a map, so it builds a provisional one from what is knowable
  without asking: all 512 low gigabyte blocks Device (Device permits no
  speculation, so mapping unknown space is harmless, and the UART answers at
  `phys_to_virt(base)` immediately), with the kernel's own block and the DTB's
  block Normal. `mmu::init` refines it break-before-make once the tree is read,
  and a throwaway `TTBR0` identity map covers only the instructions between
  `SCTLR.M` and the branch to the link address before being dropped.

  The payoff is direct: an `AddressSpace` now contains the task's pages and
  nothing else — no kernel mappings are copied into it, because a `TTBR0` switch
  no longer touches the kernel — and physical memory stopped competing with user
  virtual addresses for the same numbers, which is worth **1024 MiB of extra
  usable RAM on `-m 2G`** (895 → 1919 MiB). Verified live across `-m 128M/512M/2G`
  and both GIC versions on one unchanged image: the full seven-process demo, IPC
  with capability delegation, the user-space IRQ driver, EL0 fault isolation,
  runtime `Spawn` and frame reclaim all still run.

- **Full page tables: the EL0 window is a sparse 48-bit space, and the tables are
  the only record of it.** `AddressSpace` walks L0→L3 creating intermediate tables
  **lazily**, so a space pays a frame per table only for the regions it uses: the
  image sits at `USER_BASE`, the anonymous heap four gigabytes up, the per-process
  data page at sixteen and the stack at thirty-two, and the gaps cost nothing.

  Ownership is not a list beside the tables — it is a bit *in* them. Bits [58:55]
  of a descriptor are reserved for software, so a leaf whose frame the space
  allocated carries `SW_OWNED`, and teardown walks the tree freeing every table
  and every owned leaf. That is strictly better than the fixed `[u64; 32]` array
  it replaced: a side list is a second source of truth that can drift from the
  mappings it describes, and it caps how many pages a process may ever have. It
  also removes a special case — a mapped MMIO page simply never gets the bit, so
  teardown cannot hand a device's registers to the frame allocator — and it let
  `AddressSpace` stay a `Copy` handle, so the scheduler was untouched.

  The same walk fixed a real hole. `user_ptr_ok` used to accept any pointer inside
  `[USER_BASE, USER_BASE + 2 MiB)`, which was only *nearly* true even then — an
  unmapped address inside the window would fault the kernel at EL1 as it tried to
  honour the syscall. With the window now sparse, "inside the range" means
  nothing, so the check walks the caller's own tables and demands EL0 read rights
  (plus write rights for a store — `AP_RO_EL0` is read-only at EL1 too).

  Verified live on `-m 128M/256M/2G` and both GIC versions. The `init` image
  carries 3 MiB of `.bss` (`memsz > filesz`, so the ELF stays 9 KB) spanning two
  level-2 entries, and the memtest task reads and writes 2.5 MiB into it, then
  grows its heap by 4096 pages, touching every one; a one-off run with that target
  raised to 256 MiB on `-m 2G` also passes end to end. Reclamation is checked
  across the *whole pool* rather than by one lucky frame:
  `BuddyFrameAllocator::largest_free_run` equals the pool size only if every frame
  came back and coalesced, and it reads `64 MiB -> 64 MiB — every frame returned`
  after seven processes each mapped a 3 MiB image and one grew a 16 MiB heap.
  (That check earned its keep immediately by reporting a shortfall that turned out
  to be the UART driver still blocked in `Wait`, legitimately holding its memory —
  `start` returns when nothing is *runnable*, not when nothing is alive, so the
  report now asks `sched::live_spaces()` before blaming the allocator.)

- **PSCI, and the other cores.** A phone hands the kernel one core and holds the
  other seven in reset; the only door is PSCI, a calling convention over a single
  trapping instruction. `arch::psci` implements `VERSION`, `CPU_ON`, `CPU_OFF`,
  `AFFINITY_INFO`, `SYSTEM_OFF` and `SYSTEM_RESET` — the last two being the reset
  and power buttons of a device whose real ones you cannot reach. The kernel now
  ends its demo with `SYSTEM_OFF`, so QEMU exits by itself instead of being killed
  by a timeout.

  **The conduit comes from the device tree, and that is load-bearing rather than
  pedantic**: the same `virt` declares `method = "hvc"` with no EL2 and
  `method = "smc"` with `virtualization=on`, because once EL2 is ours an `hvc`
  from EL1 would trap into our own nonexistent EL2 vectors and the firmware has
  moved up to EL3. A hard-coded conduit would work in exactly one of the two.

  Secondaries enter at `_secondary_start` — a separate entry point, because
  firmware holds them until asked rather than letting them fall into `_start` —
  and arrive exactly as the primary did: MMU off, PC physical, possibly at EL2.
  They reuse the primary's tables, identity map included: the primary stopped
  *using* TTBR0 but never tore the tables down, precisely so this path could
  borrow them. The EL2→EL1 drop and the MMU enable are shared subroutines,
  `bl`/`ret` through x30 because neither core has a stack yet. Each core gets its
  own `.bss` stack and its id in `TPIDR_EL1` — the architecture's per-core scratch
  register, which answers "who am I" with no lock and no lookup, which is what
  makes it usable from the code that is about to *take* a lock.

  `kernel::sync::SpinLock` is a ticket lock (fair: a `compare_exchange` flag can
  starve a core indefinitely, and in a scheduler that is a task that never runs)
  that masks interrupts on the holding core — not an SMP concern but a
  single-core one, since a handler that takes a lock its own core holds waits for
  itself forever.

  Verified on `-smp 1/4/8` × GICv2/GICv3 × virtualisation on/off:
  `4 cores x 20000 locked increments = 80000 (expected 80000) — no increments
  lost`, and 160000 on eight. The counter is deliberately a plain `u64` rather
  than an atomic — an atomic would be correct *without* the lock, making it a test
  of the hardware instead of the lock — and the test is falsifiable: a one-off run
  with the lock removed produced `35200 of 80000 — LOST INCREMENTS`, which is also
  the proof that the cores genuinely execute at the same time.

- **EL0 tasks run on every core.** Six subsystems whose soundness rested on "one
  core exists" — `mem`, `heap` (the `#[global_allocator]`), `obj`, `notify`,
  `ipc`, `sched` — are now behind `SpinLock`s, plus a seventh the roadmap never
  mentioned: the console.

  Two rules make that safe rather than a new class of hang. **The lock order is a
  straight line**: `ipc` and `notify` decide under their own lock and touch the
  scheduler only after dropping it (`notify::signal` was restructured for this),
  and the single deliberate nesting — `map_anon_current` holding `sched` while
  taking `mem` — is always in that order and never the reverse. **A guard never
  outlives a `context_switch`**: it would leave the lock owned by a task that is
  no longer running, and every other core would wait on it forever. Interrupt
  masking is a separate concern that *must* span the switch, so callers mask
  around the whole operation and the lock's own masking nests inside.

  `current` and the bootstrap context are per-core. `pick_next` only returns a
  `Ready` slot and the picker marks it `Running` before dropping the guard, so two
  cores cannot claim the same task. `start` became a loop: returning to the
  bootstrap context means only "this core has nothing ready *now*", not "the
  system is finished" — another core may still be producing work — so it exits
  only when nothing is `Ready` or `Running` anywhere. And `VBAR_EL1` is per-core:
  each secondary calls `exceptions::init()` before it may be given a task, or the
  first `svc` from EL0 on that core would vector into whatever the register reset
  to.

  Verified on `-smp 1/4/8` × GICv2/GICv3 × virtualisation on/off, six consecutive
  4-core runs with no hang: the full demo — seven processes, IPC with capability
  delegation, cross-task revocation, EL0 fault isolation, runtime `Spawn`, frame
  reclaim — all still runs. The clearest evidence the tasks are genuinely
  parallel was the log itself: before the console had a lock, two EL0 processes
  printing from two cores shredded each other's lines character by character.

- **Every core is preempted by its own timer.** `gic::init` split into a
  machine-wide half (the distributor, once) and `gic::init_cpu`, which every core
  runs for itself: on GICv2 the banked CPU interface (`GICC_PMR`/`GICC_CTLR`), on
  GICv3 the redistributor (`GICR_WAKER` wake, `GICR_IGROUPR0`) plus the
  `ICC_SRE_EL1`/`ICC_PMR_EL1`/`ICC_IGRPEN1_EL1` system registers. The timer is
  per-core hardware and its interrupt is a PPI — a *private* line enabled in that
  core's own redistributor — so each core also enables and arms it itself.

  Splitting this exposed a real bug: `Gicv3` held a single `rd`, the primary's
  redistributor. A redistributor is per-core hardware and SGIs/PPIs are
  configured there, so a secondary enabling "its" timer would have enabled the
  primary's — a fault that presents as "the timer just never fires on core 1".
  It is now `[AtomicUsize; MAX_CPUS]`, each core finding its own frame by
  matching its affinity.

  `preemption: timer ticks per core — cpu0=53 cpu1=2 cpu2=2 cpu3=2` on both GIC
  versions, and the claim is falsifiable: a one-off run with `gic::init_cpu`
  removed from the secondaries produced **2 ticks instead of 64**, the primary's
  alone. Both runs *completed the demo* — it is syscall-dense, so cooperative
  switching carries it either way, which is exactly why the per-core counter had
  to exist. A single total would not have shown it: a core with no GIC simply
  contributes nothing while the number keeps climbing. (A core that never gets a
  task — eight cores, seven tasks — sits in its bootstrap loop with interrupts
  masked and reads zero, so the counter means "this core was preempted while
  running tasks", not "this core could be".)

- **Nothing about the machine is a constant any more.** The phase-1 goal was a
  kernel with no hard-coded addresses, and finishing it meant auditing rather than
  assuming. Two real violations survived until now: the UART *capability objects*
  were created with `phys: 0x0900_0000` even though the console had already
  resolved the address from the tree, and the UART's interrupt was `33`. Both now
  come from the `arm,pl011` node, and `TIMER_INTID` comes from the second entry of
  `arm,armv8-timer` (the binding fixes the order: secure physical, **non-secure
  physical**, virtual, hypervisor). Decoding `interrupts` lives in `fdt`, host-
  tested against real dumped blobs, so the tests themselves record where 33 and 30
  come from. What is left — `PL011_BASE`, `GICD_BASE`, `GICC_BASE`, the `*_FALLBACK`
  ids — is only reached when the tree is unusable, existing so there is something
  to print the complaint on; each says in its own docs that it is a lie on real
  hardware. Proven by falsification: with the fallbacks set to nonsense the UART
  driver still wakes and the timer still ticks.

- **`TCR_EL1.IPS` is read from the CPU, and the constant it replaced was wrong.**
  It was `0b010` — 40-bit — and cortex-a72 reports **44**. It never bit only
  because the linear map reaches 512 GiB (39 bits). The value genuinely varies:
  a53 → 40, a57/a72 → 44, `-cpu max` → 52. `ID_AA64MMFR0_EL1.PARange` shares its
  encoding with `IPS`, so the boot stub copies the field across, capped at 48 —
  the ceiling of a 4 KiB granule without FEAT_LPA2. The boot line prints it, so
  the guess-free path is visible rather than claimed.

- **`MAIR_EL1` names four memory types, not two.** Normal write-back, Device-nGnRE
  (MMIO), Device-nGnRnE, and Normal non-cacheable for the DMA buffers a driver
  will eventually need. MMIO stays on `nGnRE` rather than the strictest type on
  purpose: it is what Linux's `ioremap` uses, and the early write acknowledgement
  is what keeps a driver from stalling on every store to a FIFO. QEMU treats these
  alike; hardware does not.

- **The page tables are invalidated out of the D-cache before translation comes
  on.** They are written with the cache off, so they reach memory — but a stale
  dirty line left by the bootloader could evict later and rewrite them, and the
  table walker reads them *through* the cache the moment the MMU is on (`TCR` asks
  for cacheable walks). The stub runs `dc ivac` over
  `__boot_tables_start..__boot_tables_end` — invalidate, not clean: memory holds
  the truth. The line size comes from `CTR_EL0.DminLine`.

- **Cross-core wake IPI (SGI) + `wfi` idle.** The kernel can now signal another
  core: `gic::send_sgi_all_but_self` writes `GICD_SGIR` on GICv2 and
  `ICC_SGI1R_EL1` on GICv3, both after a `dsb ish` so the woken core sees the
  newly-`Ready` task. `spawn_user`/`unblock`/`deliver` ring this doorbell after
  dropping the scheduler lock; an idle core drops to `wfi` (IRQs enabled only across
  it) instead of spinning, and wakes on the IPI or, at worst, its next 10 Hz tick —
  so a missed wake costs one tick, never a hang. A `set_ttbr0` context switch now
  does a **local** `tlbi vmalle1` (a TTBR0 change affects only this core); the
  broadcast `...is` variants are kept for the unmap paths, where hardware does the
  shootdown. Falsifiable: the primary broadcasts an SGI and confirms every
  secondary's per-core IPI count moved — "every core signalled".

- **The scheduler is SMP-correct across the switch, not just the pick.** A subtle
  pre-existing race (surfaced once reaping raised the churn): a task was made
  *pickable by another core before `context_switch` had saved its context* —
  `reschedule` flips it `Ready`, or a concurrent `unblock` does, under the lock, but
  the register/SP save happens in the switch *after* the lock drops. A peer picking
  it in that window loaded a stale SP/LR and resumed on a superseded stack — a wild
  PC (`ec=0x22`, jumps to address 2). Fixed with a Linux-style `on_cpu` flag,
  **without** a switch-path spin (which could deadlock in a cycle): a task is `on_cpu`
  from when it is marked `Running` until its *successor* clears the flag after the
  switch has saved its context, and `pickable = Ready && !on_cpu` so a task still
  being saved is simply skipped and taken on a later scan. Verified 30/30 (gicv3)
  and 10/10 (gicv2) on 4 cores where the old code failed ~7–21%.

- **Dead-task stack reaping.** `exit` marks the slot `Dead` and hands it to a
  per-cpu `PREV` slot; whichever context this core resumes into frees the dead
  task's 32 KiB kernel stack in `post_switch`, at the one safe point — the successor
  is, by definition, off the dead stack. The small `Task` tombstone stays so slot
  indices and ids remain stable. Falsifiable: `reaped 13 dead-task kernel stacks
  (416 KiB returned to the heap)`, and the teardown `every frame returned` check
  still holds.

- **Line-atomic user-space console.** `DebugWrite` (a pointer+length syscall,
  capped at 4 KiB, range-checked by walking the caller's tables) emits a whole line
  under one hold of the console lock — to the UART and the framebuffer mirror both.
  The old per-byte `DebugPutc` let two EL0 tasks on two cores shred each other's
  lines; whole lines no longer tear. The 8x8 font's bit order is pinned by host
  tests; non-ASCII bytes render as a visible box so nothing silently vanishes.

- **Framebuffer console.** `crates/framebuffer` is a portable, host-tested 8x8 text
  console over a linear framebuffer (the bug-prone addressing — pitch, bpp, channel
  shifts, scroll — pinned by pixel-exact tests). The kernel mirrors the console to
  it. Two sources feed the same `FramebufferInfo`: QEMU `ramfb` over fw_cfg (the
  live test vehicle — `-device ramfb`, proven by QMP screendump), and — for a real
  Raspberry Pi, whose first output is a framebuffer not a UART (RP1 puts the GPIO
  UART behind PCIe) — the **VideoCore mailbox** (`crates/videocore` builds/parses the
  property-tag message; `arch::mailbox` is the doorbell). The mailbox path is
  compile-checked and host-tested but only runs on a board with the mailbox node;
  QEMU `virt` has none, so it falls through to ramfb.

- **PAN + unprivileged user copies.** The kernel reaches EL0 memory only through
  `arch::usercopy` (`LDTRB`/`STTRB` — unprivileged loads/stores that access *as if
  at EL0*, honouring EL0 permissions and unaffected by PAN), so it is correct on a
  part with **Privileged Access Never** (a real Cortex-A76) as well as one without
  (QEMU `cortex-a72`). PAN is then enabled where implemented (`ID_AA64MMFR1.PAN`;
  clear `SCTLR.SPAN` + `MSR PAN,#1` via raw encoding since the mnemonic needs a
  target feature the bare-metal target lacks) as defence in depth — a stray direct
  dereference of a user pointer now faults instead of leaking. Falsifiable: the
  whole IPC/cap-transfer/DebugWrite demo runs cleanly on `-cpu max` *with PAN on*,
  which a surviving privileged dereference would have faulted.

- **IOMMU (SMMUv3) enforced end-to-end against a real bus master.** The SMMU comes
  up default-deny (`GBPA.ABORT`); `bind_stream`/`bind_stream_at` install a
  stage-2-translate STE mapping exactly a device's DMA buffer (identity, or an
  explicit IOVA→PA when the device cannot emit high physical addresses). Proven not
  just by host-tested bit-layouts (`crates/iommu`) but **live**: with QEMU's `edu`
  PCIe DMA device (reached via a minimal ECAM enumerator, `arch::pci`), the kernel
  maps a low IOVA to a high buffer, and the device DMAs a pattern *through* the
  mapped page (it arrives) then to an *unmapped* IOVA (the SMMU aborts it — the page
  keeps its sentinel). Exercising it this way caught two real STE defects the
  bit-tests could not: QEMU rejected a 48-bit-IPA STE as `C_BAD_STE` (fixed →
  40-bit IPA / 44-bit OAS), and `edu`'s 28-bit DMA mask made an identity map
  impossible (fixed → non-identity IOVA→PA mapping).

Not yet implemented:

- **CPU errata and the bootloader's watchdog are untestable here and are not
  claimed.** Which errata matter depends on the SoC (Linux's
  `arch/arm64/kernel/cpu_errata.c` for the model); QEMU has none. A watchdog armed
  by the bootloader is what reboots a real device half a minute in, and is the
  first thing to look for when "the kernel booted and then died". The one thing
  already in place is linking with `--fix-cortex-a53-843419`, the default for
  `aarch64-unknown-none`.
- No load balancing: tasks stay where they are picked, and there is no work
  stealing. Cross-core TLB shootdown for unmapping uses the broadcast `tlbi ...is`
  variants (hardware agrees), which is enough for what unmaps today.
- The race test counts in the kernel rather than over IPC. The IPC path across
  cores works and the demo exercises it, but there is no dedicated IPC-level
  contention test.
- Demand paging: the stack is `USER_STACK_PAGES` mapped up front and a fault below
  it is a fault, not a request for more.
- The `edu` end-to-end IOMMU proof does not yet read the SMMU **event queue** for
  the fault record (the guest-error log plus the untouched sentinel already prove
  the abort); the ECAM enumerator is bus-0, single-function, no bridges.
- A second arch backend to validate the HAL boundary.
