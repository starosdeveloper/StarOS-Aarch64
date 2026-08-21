#!/usr/bin/env python3
"""Wait for the guest's input driver to be ready, then press a key and click on it.

QEMU's `input-send-event` synthesises an event on the emulated keyboard exactly as
a physical one would produce: the device writes it into the driver's virtqueue and
raises its interrupt. Nothing about the path is faked on the guest side, which is
the whole point — the driver in EL0 either decodes a real device's event or it
does not.

The key is sent repeatedly because there is no way from here to know when the
guest's driver has finished arming its queue; a press that arrives before then is
simply dropped by the device, and the next one lands.

The pointer is moved and clicked in the same loop, over a handful of screen
positions where this boot is known to put windows. It sweeps rather than aiming
because this script has no picture of the screen — that is `qml-verify.py`'s job,
which finds its target in the pixels and clicks its centre. What is being proved
here is the layer below that one: a tablet's absolute position crossing the
virtqueue, being normalised by a driver that does not know the screen size, and
being turned back into pixels and hit-tested by a compositor that does not know the
tablet's range.

Usage: input-send.py <qmp-socket>
"""
import json
import socket
import sys
import time


def main():
    sock_path = sys.argv[1]
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
        print("input-send: could not connect to QMP")
        sys.exit(2)
    f = s.makefile("rw")

    def cmd(obj):
        f.write(json.dumps(obj) + "\n")
        f.flush()
        return json.loads(f.readline())

    f.readline()
    cmd({"execute": "qmp_capabilities"})

    # 'a' — Linux input code 30, which is what the driver prints.
    press = {
        "execute": "input-send-event",
        "arguments": {
            "events": [{"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": "a"}}}]
        },
    }
    release = {
        "execute": "input-send-event",
        "arguments": {
            "events": [{"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": "a"}}}]
        },
    }

    # Where the windows are in this boot, in pixels of a 640x480 `ramfb` screen: the
    # two squares `fbclient` composites and the one it raises over them. Written here
    # rather than discovered because this script never looks at the screen — and a
    # position that misses every window is not a failure of the input path, so the
    # sweep visits several and the check asks only that one of them landed.
    SCREEN = (640, 480)
    ABS_MAX = 0x7FFF
    spots = [(320, 320), (120, 100), (160, 130)]

    def move_and_click(x, y):
        ax = x * ABS_MAX // (SCREEN[0] - 1)
        ay = y * ABS_MAX // (SCREEN[1] - 1)
        # The move as one event group, so the device emits ABS_X, ABS_Y and a SYN
        # before any button — the order a real tablet produces, and the order the
        # driver assembles a whole position from.
        cmd({"execute": "input-send-event", "arguments": {"events": [
            {"type": "abs", "data": {"axis": "x", "value": ax}},
            {"type": "abs", "data": {"axis": "y", "value": ay}}]}})
        cmd({"execute": "input-send-event", "arguments": {"events": [
            {"type": "btn", "data": {"down": True, "button": "left"}}]}})
        cmd({"execute": "input-send-event", "arguments": {"events": [
            {"type": "btn", "data": {"down": False, "button": "left"}}]}})

    sent = 0
    clicked = 0
    for i in range(60):
        try:
            r = cmd(press)
            cmd(release)
            move_and_click(*spots[i % len(spots)])
        except Exception:
            break
        if "error" in r:
            print(f"input-send: QMP refused the event: {r['error']}")
            break
        sent += 1
        clicked += 1
        time.sleep(0.25)

    print(f"input-send: sent {sent} key press(es) and {clicked} click(s)")
    try:
        f.close()
        s.close()
    except OSError:
        pass


if __name__ == "__main__":
    main()
