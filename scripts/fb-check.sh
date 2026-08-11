#!/usr/bin/env bash
# Assert on the *pixels* the display server put on the screen.
#
# The smoke matrix asserts on lines of text, which is the right tool for almost
# everything here and the wrong one for a compositor: a display server that prints
# "composited" and draws nothing would pass every assertion in that file. So this
# boots the kernel with `ramfb`, takes QMP screendumps while it runs, and checks
# named coordinates — the client's red square where the client asked for it, the
# server's background around it, and no kernel console text left in the region the
# server owns.
#
# Kept out of smoke-test.sh on purpose: it needs python3 and QMP, and it is the
# only check here that cares what the screen *looks* like.
#
# Usage: fb-check.sh          (exit 0 = the pixels agree)
set -uo pipefail
cd "$(dirname "$0")/.."

TMP="$(mktemp -d)"
QMP="/tmp/staros-fb-check.sock"   # short path: AF_UNIX names are capped at 108 bytes
KEEP="${1:-$PWD/composited.png}"
trap 'rm -rf "$TMP" "$QMP"' EXIT

echo "building the kernel…"
cargo kbuild >/dev/null 2>&1 || { echo "build failed"; cargo kbuild; exit 1; }

ELF="target/aarch64-unknown-none/debug/kernel"
objcopy="$(ls "$(rustc --print sysroot)"/lib/rustlib/*/bin/llvm-objcopy 2>/dev/null | head -1 || true)"
[ -z "$objcopy" ] && objcopy="$(command -v llvm-objcopy || true)"
[ -z "$objcopy" ] && { echo "no llvm-objcopy (rustup component add llvm-tools)"; exit 1; }
"$objcopy" -O binary "$ELF" "$TMP/Image"

rm -f "$QMP"
echo "booting with ramfb and checking the composited frame…"
python3 scripts/fb-verify.py "$QMP" "$TMP" >"$TMP/verify.out" 2>&1 &
VERIFY=$!

printf '\n' | timeout -k 5 60 qemu-system-aarch64 \
    -M virt,gic-version=3,virtualization=on -cpu max -smp 4 -m 512M \
    -display none -device ramfb \
    -kernel "$TMP/Image" \
    -qmp "unix:$QMP,server,nowait" \
    -serial file:"$TMP/serial.log" >/dev/null 2>&1

wait $VERIFY
RC=$?
cat "$TMP/verify.out"

echo
echo "serial log said:"
grep -a -E "handed to displaysrv|\[displaysrv\]|\[fbclient\]" "$TMP/serial.log" | sed 's/^/  /'

if [ $RC -ne 0 ]; then
    echo
    echo "the pixels did not agree — the frame is not what the two programs claim to have drawn"
    exit $RC
fi

# Keep the frame that passed where the caller can open it.
for f in "$TMP/composited.png" "$TMP/composited.ppm"; do
    if [ -f "$f" ]; then
        cp "$f" "${KEEP%.png}${f##*composited}"
        echo
        echo "the frame that passed:   xdg-open \"${KEEP%.png}${f##*composited}\""
        break
    fi
done
exit 0
