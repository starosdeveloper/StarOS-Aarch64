#!/usr/bin/env python3
"""Find the frame in which the QML scene is on the screen, and check its pixels.

`scripts/fb-verify.py` is the same idea for `displaysrv`'s own clients: named
coordinates, checked against what the programs claim to have drawn. This one asks a
different question, and it is the criterion the roadmap wrote for this phase —
*`Rectangle { color: "red" }` is visible on the QEMU screen* — which no line of text
can answer.

The scene is `services/shell/Main.qml`: a red card with a white border and white
text near the top of a 320x240 window, a blue circle that slides along a track, and a
yellow bar. The window lands wherever the compositor puts it, and this is deliberate
— the check looks for the *shape* of the scene rather than for absolute coordinates,
because the position is `displaysrv`'s decision and not QML's.

What it therefore proves, and what it does not:

  * red pixels in quantity, in a solid horizontal run  →  the software scene graph
    reached the backing store and the compositor, and QML's `"red"` survived every
    conversion between the string and the framebuffer's channel order
  * white pixels inside the red run                    →  FreeType drew glyphs from
    the fonts in the initramfs, and the border is where a border should be
  * blue and yellow present                            →  the animated items exist;
    *that they moved* is the log's claim, not this one — a single frame cannot show
    motion and pretending otherwise would be the easiest lie in this tree

Usage: qml-verify.py <qmp-socket> <tmpdir>
"""
import json
import os
import socket
import sys
import time

# The colours `Main.qml` names, and how far a pixel may be from them.
#
# A tolerance at all, because the frames are xRGB8888 and the scene graph antialiases
# edges; ±24 per channel keeps the interior of a filled rectangle and rejects
# anything that is merely reddish.
CARD_RED = (255, 0, 0)
CARD_BORDER = (240, 240, 240)
RUNNER_BLUE = (74, 120, 240)
PULSE_YELLOW = (240, 200, 80)
TOLERANCE = 24

# The card is 272 wide in a 320-wide window, so its solid rows are long. Requiring a
# run this long is what separates the card from a stray red pixel.
MIN_RED_RUN = 200
# And it is 96 tall, minus the two text lines through the middle.
MIN_RED_PIXELS = 272 * 40


def near(pixel, want, tolerance=TOLERANCE):
    return all(abs(a - b) <= tolerance for a, b in zip(pixel, want))


def parse_ppm(path):
    """Return (width, height, rows) where rows[y][x] is an (r, g, b) tuple."""
    with open(path, "rb") as fh:
        data = fh.read()
    if not data.startswith(b"P6"):
        return None
    idx, fields = 2, []
    while len(fields) < 3:
        while idx < len(data) and data[idx] in b" \t\r\n":
            idx += 1
        start = idx
        while idx < len(data) and data[idx] not in b" \t\r\n":
            idx += 1
        fields.append(int(data[start:idx]))
    width, height, _ = fields
    idx += 1
    pix = data[idx:]
    if len(pix) < width * height * 3:
        return None
    rows = []
    for y in range(height):
        base = y * width * 3
        rows.append([tuple(pix[base + 3 * x: base + 3 * x + 3]) for x in range(width)])
    return (width, height, rows)


def longest_run(row, want):
    """The longest unbroken horizontal run of `want` in `row`, and where it starts."""
    best = best_at = run = run_at = 0
    for x, pixel in enumerate(row):
        if near(pixel, want):
            if run == 0:
                run_at = x
            run += 1
            if run > best:
                best, best_at = run, run_at
        else:
            run = 0
    return best, best_at


def examine(frame):
    """What this frame shows, as a dict, or None if it is not a frame at all."""
    parsed = parse_ppm(frame)
    if parsed is None:
        return None
    width, height, rows = parsed

    red_total = 0
    best_run = best_run_at = best_run_y = 0
    for y, row in enumerate(rows):
        run, at = longest_run(row, CARD_RED)
        red_total += sum(1 for p in row if near(p, CARD_RED))
        if run > best_run:
            best_run, best_run_at, best_run_y = run, at, y

    # White inside the card: the border and the text. Counted only within the red
    # run's span and the rows around it, so the display server's own light pixels
    # elsewhere on the screen cannot stand in for glyphs that were never drawn.
    white = 0
    if best_run:
        top = max(0, best_run_y - 48)
        bottom = min(height, best_run_y + 48)
        for y in range(top, bottom):
            for x in range(best_run_at, min(width, best_run_at + best_run)):
                if near(rows[y][x], CARD_BORDER):
                    white += 1

    blue = sum(1 for row in rows for p in row if near(p, RUNNER_BLUE))
    yellow = sum(1 for row in rows for p in row if near(p, PULSE_YELLOW))

    return {
        "size": (width, height),
        "red": red_total,
        "run": best_run,
        "run_at": (best_run_at, best_run_y),
        "white": white,
        "blue": blue,
        "yellow": yellow,
    }


def main():
    sock_path, tmpdir = sys.argv[1], sys.argv[2]
    sock = None
    for _ in range(400):
        try:
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            sock.connect(sock_path)
            break
        except OSError:
            sock = None
            time.sleep(0.05)
    if sock is None:
        print("qml-verify: could not connect to QMP")
        return 2
    stream = sock.makefile("rw")

    def cmd(obj):
        stream.write(json.dumps(obj) + "\n")
        stream.flush()
        return json.loads(stream.readline())

    stream.readline()
    cmd({"execute": "qmp_capabilities"})

    # Keep the frame with the most red in it. The QML program runs for a bit over a
    # second and the shots are taken blind, so this is a search rather than a
    # measurement of one moment: whichever frame has the most of the card in it is
    # the one the scene was on screen for.
    best = None
    best_frame = None
    for i in range(220):
        frame = os.path.join(tmpdir, f"qml{i:03d}.ppm")
        try:
            reply = cmd({"execute": "screendump", "arguments": {"filename": frame}})
        except Exception:
            break
        if "error" in reply:
            break
        if os.path.exists(frame):
            seen = examine(frame)
            if seen and (best is None or seen["red"] > best["red"]):
                best, best_frame = seen, frame
            if seen and frame != best_frame:
                os.unlink(frame)
        time.sleep(0.03)

    try:
        stream.close()
        sock.close()
    except OSError:
        pass

    if best is None:
        print("qml-verify: no frame was captured at all")
        return 1

    width, height = best["size"]
    print(f"qml-verify: best frame {width}x{height}: {best['red']} red pixel(s), "
          f"longest run {best['run']} at {best['run_at']}, "
          f"{best['white']} white inside it, {best['blue']} blue, {best['yellow']} yellow")

    failures = []
    if best["red"] < MIN_RED_PIXELS:
        failures.append(f"the red card covers {best['red']} pixels, expected at least {MIN_RED_PIXELS}")
    if best["run"] < MIN_RED_RUN:
        failures.append(f"its longest solid row is {best['run']} px, expected at least {MIN_RED_RUN}"
                        " — red scattered across a frame is not a filled rectangle")
    if best["white"] < 200:
        failures.append(f"only {best['white']} white pixels inside the card: the border and the"
                        " text are what FreeType and the border draw, and neither appeared")
    if best["blue"] < 200:
        failures.append(f"only {best['blue']} blue pixels: the animated circle is missing")
    if best["yellow"] < 100:
        failures.append(f"only {best['yellow']} yellow pixels: the pulsing bar is missing")

    if failures:
        print("qml-verify: FAIL")
        for line in failures:
            print(f"  {line}")
        if best_frame:
            print(f"  the frame that failed: {best_frame}")
        return 1

    print("qml-verify: PASS - a QML scene rendered by Qt Quick's software"
          " adaptation reached the framebuffer")
    if best_frame:
        print(f"qml-verify: kept {best_frame}")
    return 0


sys.exit(main())
