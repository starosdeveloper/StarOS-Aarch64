#!/usr/bin/env bash
# Press a key on the emulated keyboard and check the driver decoded it.
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
    -display none -device virtio-keyboard-device \
    -kernel "$TMP/Image" \
    -qmp "unix:$QMP,server,nowait" \
    -serial file:"$TMP/serial.log" >/dev/null 2>&1

wait $SEND
cat "$TMP/send.out"

echo
echo "serial log said:"
grep -a -E "\[devicemgr\] (found|no virtio)|\[inputsrv\]|\[inputclient\]" "$TMP/serial.log" | sed 's/^/  /'

# The claim, in two halves. First: a key press, decoded by a driver in EL0. Code 30
# is 'a' in the Linux input numbering that virtio-input passes through unchanged.
# Second, and the one that makes input *usable*: the same code reached a different
# process, which holds no device, no interrupt and no sight of the driver's memory.
# A driver that decodes a key and tells nobody is where this stopped before.
if ! grep -qa "key press from the device: code 30" "$TMP/serial.log"; then
    echo
    echo "input-check: FAIL — no decoded key press in the log"
    exit 1
fi
if ! grep -qa "\[inputclient\] key code 30 arrived over IPC" "$TMP/serial.log"; then
    echo
    echo "input-check: FAIL — the driver decoded the key but no process received it"
    exit 1
fi

echo
echo "input-check: PASS — the key crossed device, virtqueue, interrupt, driver, and a process boundary"
exit 0
