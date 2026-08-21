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
  * an orange button that turns green when clicked     →  the whole input path, and
    this one is not an observation but an *experiment*: the click is synthesised
    here, through QMP, onto an emulated tablet, and the pixels are read back

The click is aimed at where the orange is, not at a coordinate written down here.
The window lands wherever the compositor puts it, so a fixed target would be
testing that guess. Finding the button first and clicking its centre also means the
hit test is being checked at the same time: if `displaysrv` routed by anything other
than where the surface actually is, the click lands on nothing and the button stays
orange.

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
# The button, before and after. Far apart in every channel on purpose: a click that
# reaches the window but not the item leaves it orange, and orange is not nearly
# green whichever way round the framebuffer's channels are.
BUTTON_IDLE = (255, 128, 0)
BUTTON_HIT = (0, 255, 0)
TOLERANCE = 24

# The largest value QEMU's absolute pointer axis takes. `input-send-event` scales
# this range onto the display, so a position in pixels has to be scaled *into* it —
# and the maximum is the last pixel, not one past it, which is the same off-by-one
# the compositor has at the other end of the same wire.
QEMU_ABS_MAX = 0x7FFF

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


def bounds_of(rows, want):
    """The bounding box of every pixel near `want`, or None if there are none."""
    x0 = y0 = None
    x1 = y1 = -1
    for y, row in enumerate(rows):
        for x, pixel in enumerate(row):
            if not near(pixel, want):
                continue
            if x0 is None or x < x0:
                x0 = x
            if y0 is None or y < y0:
                y0 = y
            x1 = max(x1, x)
            y1 = max(y1, y)
    if x0 is None:
        return None
    return (x0, y0, x1, y1)


def count_in(rows, box, want):
    """How many pixels inside `box` are near `want`."""
    x0, y0, x1, y1 = box
    return sum(
        1
        for y in range(y0, y1 + 1)
        for x in range(x0, x1 + 1)
        if near(rows[y][x], want)
    )


def examine(width, height, rows):
    """What this frame shows, as a dict."""
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

    def send(events):
        # The guest can power off between the frame that showed the button and the
        # command that clicks it, and then this socket is gone. That is a run that
        # ended early, not a click that failed — reported as "the button never went
        # green", which is what the caller sees anyway. A traceback here would say
        # `BrokenPipeError` and bury the actual result.
        try:
            cmd({"execute": "input-send-event", "arguments": {"events": events}})
        except (OSError, ValueError):
            pass

    def click(x, y, width, height):
        """Move the emulated tablet to (x, y) in pixels and press and release."""
        # The move is its own event group, so the device emits ABS_X, ABS_Y and a
        # SYN before any button — which is the order a real tablet produces and the
        # order the driver assembles a position from.
        ax = x * QEMU_ABS_MAX // max(1, width - 1)
        ay = y * QEMU_ABS_MAX // max(1, height - 1)
        send([{"type": "abs", "data": {"axis": "x", "value": ax}},
              {"type": "abs", "data": {"axis": "y", "value": ay}}])
        send([{"type": "btn", "data": {"down": True, "button": "left"}}])
        send([{"type": "btn", "data": {"down": False, "button": "left"}}])

    # Keep the frame with the most red in it. The QML program runs for a bit over a
    # second and the shots are taken blind, so this is a search rather than a
    # measurement of one moment: whichever frame has the most of the card in it is
    # the one the scene was on screen for.
    best = None
    best_frame = None
    # The button: where it was found, and the most green ever seen inside that
    # rectangle afterwards. Both are needed — "the button was never on screen" and
    # "it was, and the click did nothing" are different failures, and a check that
    # reports one number cannot tell them apart.
    button_box = None
    button_pixels = 0
    green_after = 0
    clicked = False
    for i in range(220):
        frame = os.path.join(tmpdir, f"qml{i:03d}.ppm")
        try:
            reply = cmd({"execute": "screendump", "arguments": {"filename": frame}})
        except Exception:
            break
        if "error" in reply:
            break
        if os.path.exists(frame):
            parsed = parse_ppm(frame)
            if parsed is not None:
                width, height, rows = parsed
                seen = examine(width, height, rows)
                if best is None or seen["red"] > best["red"]:
                    best, best_frame = seen, frame

                if not clicked:
                    box = bounds_of(rows, BUTTON_IDLE)
                    # Big enough to be the button and not a stray warm pixel: it is
                    # 272 by 48 in a scene that is 320 wide.
                    if box is not None and box[2] - box[0] > 200 and box[3] - box[1] > 30:
                        button_box = box
                        button_pixels = count_in(rows, box, BUTTON_IDLE)
                        centre_x = (box[0] + box[2]) // 2
                        centre_y = (box[1] + box[3]) // 2
                        click(centre_x, centre_y, width, height)
                        # And a keystroke, which travels the same wire and is routed
                        # by a different rule at the other end: the compositor sends
                        # a key to whoever claimed the keyboard, and a click to
                        # whatever is under the pointer. One message proves one rule.
                        try:
                            cmd({"execute": "send-key",
                                 "arguments": {"keys": [{"type": "qcode", "data": "a"}]}})
                        except (OSError, ValueError):
                            pass
                        clicked = True
                        print(f"qml-verify: clicked ({centre_x}, {centre_y}) — the centre of"
                              f" {button_pixels} orange pixel(s)")
                elif button_box is not None:
                    green_after = max(green_after, count_in(rows, button_box, BUTTON_HIT))

                if frame != best_frame:
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

    print(f"qml-verify: the button was {button_pixels} orange pixel(s) before the click"
          f" and {green_after} green pixel(s) in the same rectangle after it")
    if button_box is None:
        failures.append("the button never appeared on screen, so nothing was clicked —"
                        " this is a scene that did not draw, not an input path that failed")
    elif green_after < 2000:
        failures.append(f"the button is still orange: {green_after} green pixels where the"
                        " whole rectangle should be. The click was synthesised on the"
                        " tablet, so what did not arrive is somewhere between the"
                        " virtqueue and the MouseArea")

    if failures:
        print("qml-verify: FAIL")
        for line in failures:
            print(f"  {line}")
        if best_frame:
            print(f"  the frame that failed: {best_frame}")
        return 1

    print("qml-verify: PASS - a QML scene rendered by Qt Quick's software"
          " adaptation reached the framebuffer, and a click on an emulated tablet"
          " reached a MouseArea inside it")
    if best_frame:
        print(f"qml-verify: kept {best_frame}")
    return 0


sys.exit(main())
