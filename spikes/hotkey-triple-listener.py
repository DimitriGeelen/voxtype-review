#!/usr/bin/env python3
"""hotkey-triple-listener.py — triple-press RIGHTCTRL opens the voxtype-review
settings window (no dictation, nothing pasted).

EVDEV + select(): reads every /dev/input/event* device via select() so no
single device can starve the others (a blocking read loop made the first
version completely deaf). Runs alongside the voxtype daemon; both receive the
same kernel events. Needs the input group — start via:
    sg input -c 'python3 .../hotkey-triple-listener.py'
"""

import os
import select
import struct
import subprocess
import time
import glob

TRIPLE_WINDOW = 2.0
KEY_RIGHTCTRL = 97
EV_KEY = 1
PRESS = 1
EVENT_FMT = "llHHI"
EVENT_SIZE = struct.calcsize(EVENT_FMT)

HOME = os.path.expanduser("~")
ENV = {
    **os.environ,
    "DISPLAY": os.environ.get("DISPLAY", ":0"),
    "XAUTHORITY": os.environ.get("XAUTHORITY", f"{HOME}/.Xauthority"),
}


def is_triple(presses: list, now: float) -> bool:
    """True when the three most recent presses all fall inside the window.
    `presses` is chronological (oldest first), as the edge detector appends."""
    return len(presses) >= 3 and (now - presses[-3]) <= TRIPLE_WINDOW


def open_devices():
    files = []
    for p in sorted(glob.glob("/dev/input/event*")):
        try:
            files.append(open(p, "rb", buffering=0))
        except PermissionError:
            continue
    return files


def spawn_settings():
    subprocess.Popen(
        [f"{HOME}/.local/bin/voxtype-review", "--settings"],
        env=ENV,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )


def main():
    # Singleton: two listeners would open two settings windows per triple.
    lock = f"/tmp/voxtype-hotkey-triple-{os.getuid()}.lock"
    if os.path.exists(lock):
        try:
            other = int(open(lock).read().strip())
            os.kill(other, 0)
            print(f"already running (pid {other}) — exiting", flush=True)
            return 0
        except (ValueError, ProcessLookupError, PermissionError):
            pass  # stale lock
    with open(lock, "w") as lf:
        lf.write(str(os.getpid()))

    files = open_devices()
    if not files:
        print("no /dev/input devices readable — run via: sg input -c", flush=True)
        return 1
    print(f"listening on {len(files)} devices (select)", flush=True)

    presses: list = []
    try:
        while True:
            # select() across ALL devices — a blocking read on one device
            # starves the other 24 (the deaf-listener bug, Sep 3).
            readable, _, _ = select.select(files, [], [], 0.5)
            for f in readable:
                data = f.read(EVENT_SIZE)
                if data is None or len(data) < EVENT_SIZE:
                    continue
                _, _, etype, code, value = struct.unpack(EVENT_FMT, data)
                if etype == EV_KEY and code == KEY_RIGHTCTRL and value == PRESS:
                    now = time.monotonic()
                    presses = [t for t in presses if now - t <= TRIPLE_WINDOW]
                    presses.append(now)
                    print(
                        f"RIGHTCTRL press ({len(presses)} in window)", flush=True
                    )
                    if is_triple(presses, now):
                        print("triple RIGHTCTRL — opening settings", flush=True)
                        presses = []
                        spawn_settings()
    except KeyboardInterrupt:
        try:
            os.remove(lock)
        except NameError:
            pass
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
