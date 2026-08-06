#!/usr/bin/env python3
"""Take QMP screendumps of a running QEMU while it boots, and save the frame with
the most drawn (green) text as a PNG. Used by scripts/framebuffer-demo.sh to make
the ramfb console output visible without a graphical QEMU display backend."""
import json
import os
import shutil
import socket
import subprocess
import sys
import time


def parse_ppm(path):
    with open(path, "rb") as fh:
        data = fh.read()
    if not data.startswith(b"P6"):
        return (0, 0, 0, 0)
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
    nonblack = green = 0
    for p in range(min(len(pix) // 3, w * h)):
        r, g, b = pix[3 * p], pix[3 * p + 1], pix[3 * p + 2]
        if r or g or b:
            nonblack += 1
            if g > 80 and g > r and g > b:
                green += 1
    return (w, h, nonblack, green)


def main():
    sock_path, tmpdir, out_png = sys.argv[1], sys.argv[2], sys.argv[3]
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
        print("could not connect to QMP")
        sys.exit(2)
    f = s.makefile("rw")

    def cmd(obj):
        f.write(json.dumps(obj) + "\n")
        f.flush()
        return json.loads(f.readline())

    f.readline()
    cmd({"execute": "qmp_capabilities"})

    best = None
    for i in range(80):
        ppm = os.path.join(tmpdir, f"frame{i:02d}.ppm")
        try:
            r = cmd({"execute": "screendump", "arguments": {"filename": ppm}})
        except Exception:
            break
        if "error" in r:
            break
        if os.path.exists(ppm):
            w, h, nb, gr = parse_ppm(ppm)
            if gr and (best is None or gr > best[1]):
                best = (ppm, gr)
        time.sleep(0.08)

    # QEMU closes the QMP socket when the guest powers off; drop our handles
    # before that turns into a noisy finalizer traceback.
    try:
        f.close()
        s.close()
    except OSError:
        pass

    if not best:
        print("no drawn frame captured")
        sys.exit(1)

    src = best[0]
    if shutil.which("ffmpeg"):
        subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-i", src, out_png], check=False)
    elif shutil.which("convert"):
        subprocess.run(["convert", src, out_png], check=False)
    else:
        # No converter: just keep the PPM next to the requested path.
        out_png = os.path.splitext(out_png)[0] + ".ppm"
        shutil.copy(src, out_png)
    print(f"saved {out_png} ({best[1]} green pixels)")


main()
