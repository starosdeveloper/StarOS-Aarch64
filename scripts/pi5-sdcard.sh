#!/usr/bin/env bash
# Stage a STAR OS boot on a Raspberry Pi 5 SD card.
#
# The Pi's firmware does what QEMU's `-kernel` does for us today: it loads a flat
# `Image` from the FAT boot partition and enters it per the ARM64 Linux boot
# protocol, with a device tree pointer in x0. So there is nothing to port — the
# same flattening step `qemu-run.sh` performs is all a real board needs, plus a
# `config.txt` telling the firmware what to load.
#
# This script writes to a MOUNTED boot partition, never to a raw device. Writing
# a whole-disk image would mean guessing which /dev/sdX is the card, and getting
# that wrong costs a disk. Mount the card's first partition (the FAT one) and
# pass its mount point.
#
# Usage:
#   scripts/pi5-sdcard.sh /run/media/$USER/bootfs        # stage kernel + config
#   scripts/pi5-sdcard.sh --print-config                 # just show config.txt
#
# Prepare the card first with Raspberry Pi Imager (any Raspberry Pi OS image):
# that lays down the partition table plus the firmware blobs (start*.elf,
# fixup*.dat, bcm2712-rpi-5-b.dtb) which the Pi needs before it ever looks at a
# kernel. We only add ours beside them.
set -euo pipefail

cd "$(dirname "$0")/.."

# --- the config.txt we install ----------------------------------------------
# Kept in one place so `--print-config` and the install path cannot drift.
read -r -d '' CONFIG <<'EOF' || true
# STAR OS on Raspberry Pi 5.
#
# Written by scripts/pi5-sdcard.sh. Every line here is load-bearing; see
# docs/PI5-BRINGUP.md for why each one is set and what to check if the board
# stays dark.

[pi5]
# Load our flat Image instead of a Linux kernel. The firmware enters it per the
# ARM64 Linux boot protocol with the DTB address in x0 — the same contract the
# kernel already boots under in QEMU.
kernel=kernel8.img
arm_64bit=1

# Do not let the firmware set up a KMS/DRM framebuffer. We ask for one ourselves
# over the VideoCore property mailbox (ALLOCATE_BUFFER), which is the path
# crates/videocore implements; with firmware KMS setup active the two disagree
# about who owns the display.
disable_fw_kms_setup=1

# Turn the UART on regardless of where it comes out. On Pi 5 the GPIO 14/15 UART
# is behind RP1 (PCIe) and is NOT reachable before RP1 is up — the framebuffer is
# the first output channel, not this. Enabled anyway so firmware-stage output has
# somewhere to go while bringing the board up.
enable_uart=1
uart_2ndstage=1

# Keep the firmware's own boot chatter, so a board that never reaches our kernel
# still says something.
disable_splash=0
EOF

if [ "${1:-}" = "--print-config" ]; then
    printf '%s\n' "$CONFIG"
    exit 0
fi

BOOT="${1:-}"
if [ -z "$BOOT" ]; then
    echo "usage: $0 <path-to-mounted-boot-partition> | --print-config" >&2
    exit 2
fi
if [ ! -d "$BOOT" ]; then
    echo "pi5-sdcard: '$BOOT' is not a directory" >&2
    exit 1
fi
if ! mountpoint -q "$BOOT" 2>/dev/null; then
    # Not fatal (a bind mount or a staging directory is legitimate), but say so:
    # copying a kernel into an unmounted directory is a silent no-op at boot.
    echo "pi5-sdcard: warning — '$BOOT' is not a mount point; is the card mounted?" >&2
fi
# The firmware blobs are what make this a bootable card. Their absence means the
# card was never imaged, and our kernel alone will not boot.
if [ ! -e "$BOOT/start4.elf" ] && [ ! -e "$BOOT/start.elf" ] && [ ! -e "$BOOT/config.txt" ]; then
    echo "pi5-sdcard: '$BOOT' has no firmware (start*.elf) and no config.txt." >&2
    echo "            Image the card with Raspberry Pi Imager first, then re-run." >&2
    exit 1
fi

# --- build and flatten -------------------------------------------------------
echo "building the kernel…"
cargo kbuild >/dev/null

ELF="target/aarch64-unknown-none/debug/kernel"
objcopy="$(ls "$(rustc --print sysroot)"/lib/rustlib/*/bin/llvm-objcopy 2>/dev/null | head -1 || true)"
[ -z "$objcopy" ] && objcopy="$(command -v llvm-objcopy || command -v aarch64-linux-gnu-objcopy || true)"
if [ -z "$objcopy" ]; then
    echo "pi5-sdcard: no llvm-objcopy found (try: rustup component add llvm-tools)" >&2
    exit 1
fi

IMG="$(mktemp)"
trap 'rm -f "$IMG"' EXIT
"$objcopy" -O binary "$ELF" "$IMG"

# Sanity-check the ARM64 Linux image header before trusting the card with it: a
# header the firmware cannot recognise is a black screen with no diagnosis, and
# it costs one `dd` to rule out here. Magic "ARM\x64" sits at offset 0x38.
MAGIC="$(dd if="$IMG" bs=1 skip=56 count=4 2>/dev/null | xxd -p)"
if [ "$MAGIC" != "41524d64" ]; then
    echo "pi5-sdcard: image header magic is '$MAGIC', expected '41524d64' (\"ARM\\x64\")." >&2
    echo "            The firmware would refuse this. Not copying." >&2
    exit 1
fi
echo "  image: $(wc -c <"$IMG") bytes, ARM64 boot header OK"

# --- install -----------------------------------------------------------------
# Keep whatever config.txt was there: it is the only way back to a booting
# Raspberry Pi OS if ours does nothing.
if [ -e "$BOOT/config.txt" ] && [ ! -e "$BOOT/config.txt.staros-backup" ]; then
    cp -- "$BOOT/config.txt" "$BOOT/config.txt.staros-backup"
    echo "  saved the existing config.txt as config.txt.staros-backup"
fi

cp -- "$IMG" "$BOOT/kernel8.img"
printf '%s\n' "$CONFIG" >"$BOOT/config.txt"
sync

echo "  installed kernel8.img and config.txt in $BOOT"
echo
echo "Next: unmount the card, put it in the Pi, connect micro-HDMI (port next to"
echo "USB-C) and power. The first sign of life is text on the monitor — see"
echo "docs/PI5-BRINGUP.md for what each failure mode looks like."
