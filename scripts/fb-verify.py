#!/usr/bin/env python3
"""Check what is actually on the screen after the display server has composited.

`fb-shoot.py` keeps the frame with the most green text, which is right for looking
at the kernel's console and useless here: this phase moves the screen *out* of the
kernel, so the interesting frame is the one with no text on it at all.

What this asserts, at named coordinates, is the claim the phase makes:

  * the client's 64x64 red surface is on the glass, at (100, 80), where the client
    asked for it and nowhere else;
  * around it is the display server's background, which only the server draws;
  * the kernel's green console text is *gone* from the region the server owns —
    a kernel that kept mirroring would be writing over the composited frame.

Usage: fb-verify.py <qmp-socket> <tmpdir>   (exit 0 = the pixels agree)
"""
import json
import os
import shutil
import socket
import subprocess
import sys
import time

# What the two programs paint, as they write them (see displaysrv/main.rs and the
# fbclient role in init's image.rs).
SURFACE = (0xFF, 0x00, 0x00)  # the client's red square
BACKGROUND = (0x10, 0x20, 0x30)  # the server's background
SURFACE_AT = (100, 80)
SURFACE_SIZE = 64


def parse_ppm(path):
    """Return (width, height, pixels) with pixels as a flat list of (r, g, b)."""
    with open(path, "rb") as fh:
        data = fh.read()
    if not data.startswith(b"P6"):
        return (0, 0, [])
    idx, fields = 2, []
    while len(fields) < 3:
        while idx < len(data) and data[idx] in b" \t\r\n":
            idx += 1
        start = idx
        while idx < len(data) and data[idx] not in b" \t\r\n":
            idx += 1
        fields.append(int(data[start:idx]))
    w, h, _ = fields
    idx += 1
    pix = data[idx:]
    out = [
        (pix[3 * p], pix[3 * p + 1], pix[3 * p + 2]) for p in range(min(len(pix) // 3, w * h))
    ]
    return (w, h, out)


def near(actual, expected, tol=8):
    """Colours compare with a tolerance: a framebuffer round-trips through the
    host's PPM encoder, and an exact match would be asserting on that too."""
    return all(abs(a - e) <= tol for a, e in zip(actual, expected))


def check(path):
    """Return (ok, message) for one captured frame."""
    w, h, pix = parse_ppm(path)
    if not pix or w < 320:
        return (False, "no frame")

    def at(x, y):
        return pix[y * w + x]

    sx, sy = SURFACE_AT
    n = SURFACE_SIZE
    # Four corners and the middle of the surface, so a rectangle drawn at the
    # wrong place or the wrong size fails rather than a single lucky pixel.
    probes = [
        (sx + 1, sy + 1),
        (sx + n - 2, sy + 1),
        (sx + 1, sy + n - 2),
        (sx + n - 2, sy + n - 2),
        (sx + n // 2, sy + n // 2),
    ]
    for x, y in probes:
        if not near(at(x, y), SURFACE):
            return (False, f"surface pixel ({x},{y}) is {at(x, y)}, not red")

    # Just outside every edge must be background, not surface: this is what
    # catches a blit that is one pixel too wide or offset by a row.
    for x, y in [(sx - 2, sy + n // 2), (sx + n + 1, sy + n // 2), (sx + n // 2, sy - 2)]:
        if not near(at(x, y), BACKGROUND):
            return (False, f"pixel ({x},{y}) beside the surface is {at(x, y)}, not the background")

    # And the server's background must cover the area the kernel's console used to
    # write in. Green text there means the kernel never stopped mirroring.
    greens = sum(1 for x in range(0, w, 4) for y in range(0, 40, 4) if at(x, y)[1] > 80)
    if greens:
        return (False, f"{greens} green console pixels remain in the top rows")
    return (True, f"{w}x{h}: surface at {SURFACE_AT}, background around it, no console text")


def main():
    sock_path, tmpdir = sys.argv[1], sys.argv[2]
    s = None
    for _ in range(200):
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(sock_path)
            break
        except OSError:
            s = None
            time.sleep(0.05)
    if s is None:
        print("fb-verify: could not connect to QMP")
        sys.exit(2)
    f = s.makefile("rw")

    def cmd(obj):
        f.write(json.dumps(obj) + "\n")
        f.flush()
        return json.loads(f.readline())

    f.readline()
    cmd({"execute": "qmp_capabilities"})

    # Keep sampling until the pixels agree or the guest powers off. Which frame is
    # the right one is not knowable from here — the demo runs for seconds and the
    # composite lands somewhere inside that — so every frame is a candidate and the
    # first one that passes ends it.
    verdict = "no frame captured"
    for i in range(120):
        ppm = os.path.join(tmpdir, f"verify{i:03d}.ppm")
        try:
            r = cmd({"execute": "screendump", "arguments": {"filename": ppm}})
        except Exception:
            break
        if "error" in r:
            break
        if os.path.exists(ppm):
            ok, why = check(ppm)
            if ok:
                print(f"fb-verify: PASS - {why}")
                # Keep the frame that passed, converted if the host can, so the
                # claim can be looked at rather than only read.
                keep = os.path.join(tmpdir, "composited.png")
                if shutil.which("ffmpeg"):
                    subprocess.run(
                        ["ffmpeg", "-y", "-loglevel", "error", "-i", ppm, keep], check=False
                    )
                elif shutil.which("convert"):
                    subprocess.run(["convert", ppm, keep], check=False)
                else:
                    keep = os.path.join(tmpdir, "composited.ppm")
                    shutil.copy(ppm, keep)
                print(f"fb-verify: kept {keep}")
                sys.exit(0)
            verdict = why
            os.remove(ppm)
        time.sleep(0.08)

    print(f"fb-verify: FAIL - {verdict}")
    sys.exit(1)


if __name__ == "__main__":
    main()
