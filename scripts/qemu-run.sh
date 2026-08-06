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

exec qemu-system-aarch64 \
    -M virt,gic-version=2 \
    -cpu cortex-a72 \
    -m 256M \
    -nographic \
    -kernel "$image" \
    "$@"
