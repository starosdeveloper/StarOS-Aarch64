#!/usr/bin/env bash
# Press a key on the emulated keyboard and check it reaches a window.
#
# `-device ramfb` is not decoration here. Input is routed by the *display server* —
# it is the only process that knows which window has the focus — so a machine with
# no framebuffer has no display server, and a decoded key has nowhere to go. This
# script ran without it for one revision and reported "the driver decoded the key
# but no process received it", which was true and had nothing to do with the driver.
#
# The rest of the test suite asserts on what the kernel and its programs say about
# themselves. This one makes the *outside world* do something: QEMU synthesises a
# real key event on a virtio-input device, the device writes it into a virtqueue,
# raises an interrupt, and a driver in EL0 — with no kernel code anywhere in the
# path — decodes it and prints the key code.
#
# Kept out of smoke-test.sh because it needs python3 and QMP, like fb-check.sh.
#
# Usage: input-check.sh        (exit 0 = the key arrived and was decoded)
set -uo pipefail
cd "$(dirname "$0")/.."

TMP="$(mktemp -d)"
QMP="/tmp/staros-input-qmp.sock"   # short path: AF_UNIX names are capped at 108 bytes
trap 'rm -rf "$TMP" "$QMP"' EXIT

echo "building the kernel…"
cargo kbuild >/dev/null 2>&1 || { echo "build failed"; cargo kbuild; exit 1; }

ELF="target/aarch64-unknown-none/debug/kernel"
objcopy="$(ls "$(rustc --print sysroot)"/lib/rustlib/*/bin/llvm-objcopy 2>/dev/null | head -1 || true)"
[ -z "$objcopy" ] && objcopy="$(command -v llvm-objcopy || true)"
[ -z "$objcopy" ] && { echo "no llvm-objcopy (rustup component add llvm-tools)"; exit 1; }
"$objcopy" -O binary "$ELF" "$TMP/Image"

rm -f "$QMP"
echo "booting with a virtio keyboard and pressing a key…"
python3 scripts/input-send.py "$QMP" >"$TMP/send.out" 2>&1 &
SEND=$!

printf '\n' | timeout -k 5 60 qemu-system-aarch64 \
    -M virt,gic-version=3,virtualization=on -cpu max -smp 4 -m 512M \
    -display none -device ramfb -device virtio-keyboard-device \
    -device virtio-tablet-device \
    -kernel "$TMP/Image" \
    -qmp "unix:$QMP,server,nowait" \
    -serial file:"$TMP/serial.log" >/dev/null 2>&1

wait $SEND
cat "$TMP/send.out"

echo
echo "serial log said:"
grep -a -E "\[devicemgr\] (found|no virtio)|\[inputsrv\]|\[inputclient\]|\[displaysrv\] pointer" "$TMP/serial.log" | sed 's/^/  /'

# The claim, in two halves. First: a key press, decoded by a driver in EL0. Code 30
# is 'a' in the Linux input numbering that virtio-input passes through unchanged.
# Second, and the one that makes input *usable*: the same code reached a window,
# through the display server, in a process holding no device, no interrupt and no
# sight of the driver's memory. Routed rather than broadcast — the compositor is the
# only process that knows which window has the focus, and that is where the decision
# belongs.
if ! grep -qa "key press from the device: code 30" "$TMP/serial.log"; then
    echo
    echo "input-check: FAIL — no decoded key press in the log"
    exit 1
fi
if ! grep -qa "\[inputclient\] key code 30 reached my window through displaysrv" "$TMP/serial.log"; then
    echo
    echo "input-check: FAIL — the driver decoded the key but no window received it"
    exit 1
fi

# And the third half, which is a different claim rather than more of the same one.
#
# A key is routed by *who has the keyboard*; a pointer is routed by *what is under
# it*. Those are separate decisions in the compositor and they fail separately: a
# server that routed clicks by focus passes both tests above and delivers every
# click to the wrong window. The position also crosses two conversions on the way —
# device units to a fraction in the driver, fraction to pixels here — and neither
# process holds both numbers, which is the point of splitting it that way and the
# reason getting it wrong lands the pointer somewhere plausible but not where it is.
if ! grep -qa "\[displaysrv\] pointer at (" "$TMP/serial.log"; then
    echo
    echo "input-check: FAIL — the tablet was clicked and no pointer event reached a window."
    echo "  Either the driver never assembled a position from ABS_X/ABS_Y/SYN, or the"
    echo "  compositor's hit test found nothing under a pointer that was over a window."
    exit 1
fi

echo
echo "input-check: PASS — the key crossed device, virtqueue, interrupt, driver, the compositor, and a process boundary,"
echo "                    and a click crossed the same path to the window it was over rather than the one with the focus"
exit 0
