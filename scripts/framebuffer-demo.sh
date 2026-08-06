#!/usr/bin/env bash
# See the framebuffer console for yourself.
#
# QEMU on this host has no graphical display backend compiled in (`-display help`
# lists only `none`), so we cannot pop up a live window. Instead we boot the
# kernel with `ramfb`, take QMP screendumps while the demo runs, and save the
# frame with the most on-screen text as a PNG you can open in any image viewer.
#
# Usage: framebuffer-demo.sh [output.png]   (default: ./framebuffer.png)
#
# If you WANT a live window, install a QEMU UI backend (Arch: `qemu-ui-gtk`) and
# run the plain command printed at the end instead.
set -uo pipefail
cd "$(dirname "$0")/.."

OUT="${1:-$PWD/framebuffer.png}"
TMP="$(mktemp -d)"
QMP="/tmp/staros-fb-qmp.sock"   # short path: AF_UNIX socket names are capped at 108 bytes
trap 'rm -rf "$TMP" "$QMP"' EXIT

echo "building the kernel…"
cargo kbuild >/dev/null 2>&1 || { echo "build failed"; cargo kbuild; exit 1; }

ELF="target/aarch64-unknown-none/debug/kernel"
objcopy="$(ls "$(rustc --print sysroot)"/lib/rustlib/*/bin/llvm-objcopy 2>/dev/null | head -1 || true)"
[ -z "$objcopy" ] && objcopy="$(command -v llvm-objcopy || true)"
[ -z "$objcopy" ] && { echo "no llvm-objcopy (rustup component add llvm-tools)"; exit 1; }
"$objcopy" -O binary "$ELF" "$TMP/Image"

rm -f "$QMP"
echo "booting QEMU with ramfb and capturing the screen…"
python3 scripts/fb-shoot.py "$QMP" "$TMP" "$OUT" &
SHOOT=$!

printf '\n' | timeout -k 5 40 qemu-system-aarch64 \
    -M virt,gic-version=3,virtualization=on -cpu max -smp 4 -m 512M \
    -display none -device ramfb \
    -kernel "$TMP/Image" \
    -qmp "unix:$QMP,server,nowait" \
    -serial file:"$TMP/serial.log" >/dev/null 2>&1

wait $SHOOT
echo
echo "serial log said:"
grep -a -E "framebuffer: ramfb|every frame returned|shutting down" "$TMP/serial.log" | sed 's/^/  /'
echo
echo "open the screenshot:   xdg-open \"$OUT\""
echo
echo "for a LIVE window instead: install a UI backend (Arch: sudo pacman -S qemu-ui-gtk), then:"
echo "  $objcopy -O binary $ELF /tmp/Image"
echo "  qemu-system-aarch64 -M virt,gic-version=3,virtualization=on -cpu max -smp 4 -m 512M \\"
echo "      -device ramfb -display gtk -no-shutdown -kernel /tmp/Image -serial stdio"
