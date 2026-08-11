#!/usr/bin/env bash
# Boot the kernel in QEMU the way a real bootloader would.
#
# `cargo` hands us the freshly-linked ELF. QEMU can boot that directly, but its
# ELF path jumps straight to the entry point and passes **no device tree** —
# x0 is zero, and the kernel would fall back to hard-coded addresses, silently
# exercising a path that does not exist on real hardware.
#
# So we do what the build for a real device does: flatten the ELF into an
# `Image` and let QEMU's boot stub load it per the ARM64 Linux boot protocol,
# which is what puts the DTB pointer in x0. Same protocol as an Android
# bootloader, so this path is the one worth testing every day.
#
# Usage: qemu-run.sh <kernel-elf> [extra qemu args...]
set -euo pipefail

elf="$1"
shift

# llvm-objcopy ships with the Rust toolchain, so there is no extra dependency to
# install; fall back to a cross binutils if someone has one.
objcopy="$(ls "$(rustc --print sysroot)"/lib/rustlib/*/bin/llvm-objcopy 2>/dev/null | head -1 || true)"
if [ -z "$objcopy" ]; then
    objcopy="$(command -v llvm-objcopy || command -v aarch64-linux-gnu-objcopy || true)"
fi
if [ -z "$objcopy" ]; then
    echo "qemu-run: no llvm-objcopy found (try: rustup component add llvm-tools)" >&2
    exit 1
fi

image="${elf}.img"
"$objcopy" -O binary "$elf" "$image"

# Build an initramfs beside the image, so the plain run exercises the paths that
# need one: unpacking an archive in user space, and `SpawnImage` loading a program
# out of it. Without this the machine reports "no initramfs" and two subsystems
# stay silent in the most ordinary command — which is how they went unnoticed
# before. The archive is rebuilt every run because `init.elf` changes with the
# build; if the host has no `cpio`, the run simply proceeds without one.
initrd=""
init_elf="$(ls -t "$(dirname "$elf")"/build/kernel-*/out/init.elf 2>/dev/null | head -1 || true)"
if command -v cpio >/dev/null 2>&1 && [ -n "$init_elf" ]; then
    irdir="${elf}.initrd.d"
    rm -rf "$irdir"
    mkdir -p "$irdir"
    printf 'hello from the initramfs\n' >"$irdir/greeting.txt"
    printf 'STAR OS 0.2.0\n' >"$irdir/version"
    cp "$init_elf" "$irdir/init.elf"
    initrd="${elf}.initrd.cpio"
    ( cd "$irdir" && printf '%s\n' greeting.txt version init.elf |
        cpio -o -H newc --reproducible 2>/dev/null ) >"$initrd"
fi

# The machine `cargo krun` builds is deliberately a *complete* one: a framebuffer
# to hand to the display server, a keyboard for the input driver to find, and an
# initramfs to load a program out of. Anything absent here is a subsystem that
# reports "this machine has none" and disappears from the log — which is exactly
# how the display server and the input driver each went unnoticed after being
# written. The devices cost nothing when nothing uses them.
#
# `-device ramfb` is on by default, and that is a deliberate change of what the
# plain `cargo krun` shows. A machine with no framebuffer gives the kernel no
# screen to hand over, so `displaysrv` and its client are never created and the
# most visible thing this system does is invisible in its most ordinary command.
# The device costs nothing when nothing draws (the guest allocates the buffer), and
# `-display none` keeps the run headless — the pixels are inspected with
# `scripts/fb-check.sh`, not by opening a window.
exec qemu-system-aarch64 \
    -M virt,gic-version=2 \
    -cpu cortex-a72 \
    -m 256M \
    -nographic \
    -display none \
    -device ramfb \
    -device virtio-keyboard-device \
    ${initrd:+-initrd "$initrd"} \
    -kernel "$image" \
    "$@"
