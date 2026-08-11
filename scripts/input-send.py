#!/usr/bin/env python3
"""Wait for the guest's input driver to be ready, then press a key on it.

QEMU's `input-send-event` synthesises an event on the emulated keyboard exactly as
a physical one would produce: the device writes it into the driver's virtqueue and
raises its interrupt. Nothing about the path is faked on the guest side, which is
the whole point — the driver in EL0 either decodes a real device's event or it
does not.

The key is sent repeatedly because there is no way from here to know when the
guest's driver has finished arming its queue; a press that arrives before then is
simply dropped by the device, and the next one lands.

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

    sent = 0
    for _ in range(60):
        try:
            r = cmd(press)
            cmd(release)
        except Exception:
            break
        if "error" in r:
            print(f"input-send: QMP refused the event: {r['error']}")
            break
        sent += 1
        time.sleep(0.25)

    print(f"input-send: sent {sent} key press(es)")
    try:
        f.close()
        s.close()
    except OSError:
        pass


if __name__ == "__main__":
    main()
