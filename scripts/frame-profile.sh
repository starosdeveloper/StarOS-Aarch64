#!/usr/bin/env bash
# Where a frame's time goes, measured on all three sides of it.
#
# Phase G8 opens with "first measure, then touch", and this is the measurement. It
# boots the same machine `qml-check.sh` does — ramfb, an initramfs with the scene and
# the fonts, a keyboard and a tablet — and instead of looking at pixels it reads the
# three profile lines the system prints about itself:
#
#   [shell]      the Qt Quick stages, from the signals the software render loop emits
#                at each boundary: sync, raster, present
#   [qstaros]    what `present` costs from the client's end: the commit round trip
#                and the back-buffer restore that damage tracking requires
#   [displaysrv] what the compositor spends inside that round trip, per frame and
#                per pixel
#
# Three sources and not one, because no single process can see a whole frame. The
# interesting numbers are the *differences* — the commit round trip minus the
# compositing inside it is what the kernel's IPC costs, and nothing measures that
# directly.
#
# It asserts that all three lines are there. A profile that silently reports two of
# three stages is how "rasterisation is free" gets believed.
#
# Usage: frame-profile.sh          (exit 0 = every stage was measured)
set -uo pipefail
cd "$(dirname "$0")/.."

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "building the kernel…"
cargo kbuild >/dev/null 2>&1 || { echo "build failed"; cargo kbuild; exit 1; }

ELF="target/aarch64-unknown-none/debug/kernel"
objcopy="$(ls "$(rustc --print sysroot)"/lib/rustlib/*/bin/llvm-objcopy 2>/dev/null | head -1 || true)"
[ -z "$objcopy" ] && objcopy="$(command -v llvm-objcopy || true)"
[ -z "$objcopy" ] && { echo "no llvm-objcopy (rustup component add llvm-tools)"; exit 1; }
"$objcopy" -O binary "$ELF" "$TMP/Image"

# The archive `cargo krun` builds, reused rather than rebuilt — the same decision
# `qml-check.sh` makes, and for the same reason: the scene's path would otherwise be
# written down in a second place.
INITRD="$ELF.initrd.cpio"
if [ ! -f "$INITRD" ]; then
    echo "no initramfs at $INITRD — run \`cargo krun\` once so qemu-run.sh builds it"
    exit 2
fi
if [ ! -f target/shell/shell.elf ]; then
    echo "no QML program: run scripts/qt-link.sh (Qt is built outside this tree)"
    exit 2
fi

echo "booting, and letting the scene render for its full run…"
printf '\n' | timeout -k 5 180 qemu-system-aarch64 \
    -M virt,gic-version=3,virtualization=on -cpu max -smp 4 -m 512M \
    -display none -device ramfb -device virtio-keyboard-device \
    -device virtio-tablet-device \
    -kernel "$TMP/Image" -initrd "$INITRD" \
    -serial file:"$TMP/serial.log" >/dev/null 2>&1

echo
echo "what the system said about itself:"
grep -a -E "^\[shell\] (frame profile|[0-9]+ frame)|^\[qstaros\] present|^\[displaysrv\] composite" \
    "$TMP/serial.log" | sed 's/^/  /'

# ---------------------------------------------------------------- the arithmetic
#
# One awk over the whole log rather than a grep per number: the *last* line of each
# kind is the one that covers the whole run, and picking the last is a one-line rule
# here and a pipeline of `tail -1`s otherwise.
#
# `[qstaros] present` appears twice in an ordinary boot — `qt-hello` has a window
# too, and its single frame is a cold path that would otherwise be averaged into the
# QML program's four hundred. The one with the most frames is the QML program's, and
# that is how it is chosen: by size, not by position, so the day a third Qt program
# appears the rule still names the one doing the drawing.
awk '
/^\[shell\] frame profile over/ {
    for (i = 1; i <= NF; i++) {
        if ($i == "sync")    sync    = $(i + 1)
        if ($i == "raster")  raster  = $(i + 1)
        if ($i == "present") present = $(i + 1)
    }
    frames = $5
    have_shell = 1
}
/^\[shell\] [0-9]+ frame\(s\) in/ { wall_frames = $2; wall_ms = $5; have_wall = 1 }
/^\[qstaros\] present:/ {
    if ($3 + 0 > qt_frames) {
        qt_frames = $3 + 0
        for (i = 1; i <= NF; i++) {
            if ($i == "commit")  qt_commit  = $(i + 1)
            if ($i == "restore") qt_restore = $(i + 1)
        }
        have_qt = 1
    }
}
/^\[displaysrv\] composite:/ {
    for (i = 1; i <= NF; i++) {
        if ($(i + 1) == "us/frame,") ds_frame = $i
        if ($(i + 1) == "ns/px,"   ) ds_px    = $i
        if ($(i + 1) == "ns/px"    ) ds_px    = $i
    }
    ds_frames = $3
    have_ds = 1
}
END {
    missing = 0
    if (!have_shell) { print "\nthe QML program printed no frame profile"; missing = 1 }
    if (!have_qt)    { print "\nthe plugin printed no present profile";    missing = 1 }
    if (!have_ds)    { print "\nthe display server printed no composite profile"; missing = 1 }
    if (missing) exit 1

    stage_total = sync + raster + present
    printf "\nper frame, over %d frame(s):\n\n", frames
    printf "  %-34s %8d us   %5.1f%% of the measured frame\n", "sync (QML -> scene graph)",  sync,    100 * sync    / stage_total
    printf "  %-34s %8d us   %5.1f%%\n",                        "raster (QPainter, software)", raster, 100 * raster  / stage_total
    printf "  %-34s %8d us   %5.1f%%\n",                        "present (flush to the server)", present, 100 * present / stage_total
    printf "  %-34s %8d us\n",                                  "measured total",              stage_total

    if (have_wall && wall_frames > 0) {
        interval = 1000 * wall_ms / wall_frames
        printf "  %-34s %8d us   (%.1f frame(s) a second)\n", "wall clock between frames", interval, 1000000 / interval
        printf "  %-34s %8d us   polish, animation, the event loop\n", "unaccounted", interval - stage_total
    }

    printf "\ninside present:\n\n"
    printf "  %-34s %8d us\n", "commit round trip",  qt_commit
    printf "  %-34s %8d us   over %d frame(s)\n", "of which compositing", ds_frame, ds_frames
    printf "  %-34s %8d us   kernel IPC and scheduling\n", "of which everything else", qt_commit - ds_frame
    printf "  %-34s %8d us   the back-buffer restore\n", "restore", qt_restore
    printf "\n  compositing costs %s ns per pixel written.\n", ds_px
}
' "$TMP/serial.log"
status=$?

echo
if [ $status -ne 0 ]; then
    echo "frame-profile: a stage went unmeasured — see above"
    exit 1
fi
echo "frame-profile: every stage of a frame was measured"
