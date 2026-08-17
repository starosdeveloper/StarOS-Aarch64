#!/usr/bin/env bash
# Assert on the *pixels* of the QML scene.
#
# `scripts/fb-check.sh` does this for `displaysrv`'s own clients and boots without an
# initramfs, which means no fonts and no scene file — so the Qt programs in that boot
# find nothing to draw with. This one boots the same machine *with* the archive and
# asks the one question the roadmap set as this phase's criterion: is
# `Rectangle { color: "red" }` on the screen.
#
# It is separate from `smoke-test.sh` for the same reason `fb-check.sh` is: it needs
# python3 and QMP, and it is about what the screen looks like rather than what the
# log says.
#
# Usage: qml-check.sh          (exit 0 = the scene is on the screen)
set -uo pipefail
cd "$(dirname "$0")/.."

TMP="$(mktemp -d)"
QMP="/tmp/staros-qml-check.sock"   # short path: AF_UNIX names are capped at 108 bytes
trap 'rm -rf "$TMP" "$QMP"' EXIT

echo "building the kernel…"
cargo kbuild >/dev/null 2>&1 || { echo "build failed"; cargo kbuild; exit 1; }

ELF="target/aarch64-unknown-none/debug/kernel"
objcopy="$(ls "$(rustc --print sysroot)"/lib/rustlib/*/bin/llvm-objcopy 2>/dev/null | head -1 || true)"
[ -z "$objcopy" ] && objcopy="$(command -v llvm-objcopy || true)"
[ -z "$objcopy" ] && { echo "no llvm-objcopy (rustup component add llvm-tools)"; exit 1; }
"$objcopy" -O binary "$ELF" "$TMP/Image"

# The archive `cargo krun` builds, reused rather than rebuilt: it already carries the
# fonts and `qml/Main.qml`, and building a second one here would be a second place
# for the scene's path to be written down.
INITRD="$ELF.initrd.cpio"
if [ ! -f "$INITRD" ]; then
    echo "no initramfs at $INITRD — run \`cargo krun\` once so qemu-run.sh builds it"
    exit 2
fi
if ! cpio -t <"$INITRD" 2>/dev/null | grep -q '^qml/Main.qml$'; then
    echo "the initramfs has no qml/Main.qml in it; the QML program would find nothing"
    exit 2
fi

if [ ! -f target/shell/shell.elf ]; then
    echo "no QML program: run scripts/qt-link.sh (Qt is built outside this tree)"
    exit 2
fi

rm -f "$QMP"
echo "booting with ramfb and the archive, and looking for the scene…"
python3 scripts/qml-verify.py "$QMP" "$TMP" >"$TMP/verify.out" 2>&1 &
VERIFY=$!

printf '\n' | timeout -k 5 120 qemu-system-aarch64 \
    -M virt,gic-version=3,virtualization=on -cpu max -smp 4 -m 512M \
    -display none -device ramfb -device virtio-keyboard-device \
    -kernel "$TMP/Image" -initrd "$INITRD" \
    -qmp "unix:$QMP,server,nowait" \
    -serial file:"$TMP/serial.log" >/dev/null 2>&1

wait $VERIFY
RC=$?
cat "$TMP/verify.out"

echo
echo "serial log said:"
grep -a -E "\[shell\]|\[displaysrv\] [0-9]" "$TMP/serial.log" | sed 's/^/  /'

if [ $RC -ne 0 ]; then
    echo
    echo "the pixels did not agree — the scene is not on the screen the log describes"
    exit $RC
fi

# Keep the frame that passed where the caller can open it.
KEEP="${1:-$PWD/qml-scene.png}"
BEST="$(grep -o 'kept .*' "$TMP/verify.out" | cut -d' ' -f2- || true)"
if [ -n "$BEST" ] && [ -f "$BEST" ]; then
    if command -v ffmpeg >/dev/null 2>&1; then
        ffmpeg -y -loglevel error -i "$BEST" "$KEEP" && echo && echo "the frame that passed:   xdg-open \"$KEEP\""
    else
        cp "$BEST" "${KEEP%.png}.ppm"
        echo
        echo "the frame that passed:   ${KEEP%.png}.ppm (no ffmpeg to make a PNG)"
    fi
fi
