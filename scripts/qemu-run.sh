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

# This script's own directory, so the paths below do not depend on where `cargo`
# was run from — the runner is invoked with the workspace as its cwd today and
# that is not something to rely on.
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

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
    # A subdirectory, and one below it. CPIO stores paths and not directories, so
    # these members are the only thing that makes `docs` a directory at all — and
    # they are what `opendir`/`readdir` in libc are tested against: `docs` holds one
    # file and one directory, and `docs/deep` is reported once rather than once per
    # file inside it.
    mkdir -p "$irdir/docs/deep"
    printf 'read me\n' >"$irdir/docs/readme.txt"
    printf 'down here\n' >"$irdir/docs/deep/note.txt"
    # The system font. Qt's `QFreeTypeFontDatabase` opens files by path, and this is
    # where they will be — so it is put here now, and read now, rather than becoming
    # the first thing that fails during a Qt build for a reason nobody can see.
    #
    # All fourteen weights and not just the four a toolkit resolves by default: a
    # QML `font.weight: Font.Light` that finds nothing does not fail, it silently
    # picks the nearest match, and a UI drawn in the wrong weight looks like a
    # rendering bug. Two megabytes against 124 usable, and the archive is excluded
    # from the frame pool either way.
    fonts="$here/../../IBM_Plex_Mono"
    font_members=""
    if [ -d "$fonts" ]; then
        mkdir -p "$irdir/fonts"
        cp "$fonts"/*.ttf "$fonts/OFL.txt" "$irdir/fonts/" 2>/dev/null || true
        # The licence travels with the font, which the SIL OFL requires and which is
        # also the only way anyone reading the archive can tell where it came from.
        font_members="$(cd "$irdir" && ls fonts/* 2>/dev/null || true)"
    fi
    # The QML scene `services/shell` opens. Read at run time and not compiled in, so
    # editing it changes what is on the screen at the next `cargo krun` without
    # relinking twenty-five megabytes of Qt.
    scene="$here/../services/shell/Main.qml"
    scene_member=""
    if [ -f "$scene" ]; then
        mkdir -p "$irdir/qml"
        cp "$scene" "$irdir/qml/Main.qml"
        scene_member="qml/Main.qml"
    fi
    initrd="${elf}.initrd.cpio"
    ( cd "$irdir" && printf '%s\n' greeting.txt version init.elf \
        docs/readme.txt docs/deep/note.txt $scene_member $font_members |
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
# The device costs nothing when nothing draws (the guest allocates the buffer).
#
# `-device virtio-tablet-device` is the pointer, and it is a *tablet* rather than a
# mouse because a tablet reports where it is instead of how far it moved. A mouse's
# deltas need somewhere to keep the pointer between events, and the process that
# would keep it is the compositor — so a mouse is a second definition of where the
# pointer is, living in a driver. Absolute positions have one.

# Whether this QEMU can open a window, and whether there is a session to open it in.
#
# Both halves are asked because both can be false independently, and the failure is
# the same silence either way: a screen nobody sees, a keyboard nobody types on, and
# a scene that reports `0 click(s), 0 key(s)` — which reads exactly like an input
# path that does not work.
#
# Arch splits the UI drivers into their own packages (`qemu-ui-gtk`, `qemu-ui-sdl`),
# and a `qemu-system-aarch64` without them lists **only** `none` under
# `-display help`. That is not a misconfiguration to work around; it is a fact to
# report, which is what the `else` branch below does.
ui=""
for backend in gtk sdl; do
    if qemu-system-aarch64 -display help 2>/dev/null | grep -qx "$backend"; then
        ui="$backend"
        break
    fi
done
if [ -z "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
    ui=""
fi

if [ -n "$ui" ]; then
    # A real window: the screen `displaysrv` owns, and a keyboard and tablet that
    # take what is typed and clicked into it. The serial log still comes back here —
    # `mon:stdio` keeps the console in this terminal, where every other line of
    # evidence in this project already is. Ctrl-A then X still quits.
    echo "qemu: opening a $ui window — the screen is real, and so are the keyboard and the pointer" >&2
    exec qemu-system-aarch64 \
        -M virt,gic-version=2 \
        -cpu cortex-a72 \
        -m 256M \
        -display "$ui" \
        -serial mon:stdio \
        -device ramfb \
        -device virtio-keyboard-device \
        -device virtio-tablet-device \
        ${initrd:+-initrd "$initrd"} \
        -kernel "$image" \
        "$@"
fi

# No window available. Say why, and say it *before* the boot rather than leaving the
# input counters to be read as a broken path.
if qemu-system-aarch64 -display help 2>/dev/null | grep -qx none &&
   ! qemu-system-aarch64 -display help 2>/dev/null | grep -qxE 'gtk|sdl'; then
    echo "qemu: this build has no graphical display backend (only 'none')," >&2
    echo "      so the screen cannot be shown and nothing can be typed or clicked." >&2
    echo "      The scene will report 0 clicks and 0 keys, and that is the machine," >&2
    echo "      not the input path — pacman -S qemu-ui-gtk gives it a window." >&2
    echo "      Meanwhile ./scripts/qml-check.sh synthesises a click over QMP and" >&2
    echo "      checks the pixels it changed." >&2
else
    echo "qemu: no display session (DISPLAY/WAYLAND_DISPLAY unset); running headless." >&2
fi
exec qemu-system-aarch64 \
    -M virt,gic-version=2 \
    -cpu cortex-a72 \
    -m 256M \
    -nographic \
    -display none \
    -device ramfb \
    -device virtio-keyboard-device \
    -device virtio-tablet-device \
    ${initrd:+-initrd "$initrd"} \
    -kernel "$image" \
    "$@"
