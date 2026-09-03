#!/usr/bin/env bash
# check-hotkey.sh — post-restart preflight for the voxtype daemon hotkey.
#
# This check exists because the hotkey died THREE times (Aug 31–Sep 1) after
# daemon restarts while every other signal looked green: status=idle, config
# valid, popup deploy fresh. The causes, in order of frequency:
#
#   1. user not in the `input` group (evdev cannot read /dev/input/event*)
#   2. daemon started without DISPLAY/XAUTHORITY (X11 fallback deaf)
#   3. stale voxtype.lock making the new daemon die on startup
#
# Run after EVERY daemon restart. Exit 0 = safe to dictate; non-zero = the
# hotkey will silently do nothing.

FAIL=0

say()  { printf '%s\n' "$*"; }
bad()  { say "FAIL: $*"; FAIL=1; }

# 1. daemon process alive
DPID=$(pgrep -x voxtype | head -1)
[ -n "$DPID" ] || { bad "no voxtype daemon process — start it first"; exit 1; }

# 2. status idle (not stopped/crashed)
ST=$(voxtype status 2>/dev/null | head -1)
[ "$ST" = "idle" ] || bad "voxtype status is '$ST' (expected idle)"

# 3. evdev possible: the DAEMON process must carry the input group (gid from
#    /etc/group) — session groups don't matter, /proc/PID/status is the truth
IGID=$(getent group input | cut -d: -f3)
if grep "^Groups:" "/proc/$DPID/status" 2>/dev/null | grep -qw "${IGID:-995}"; then
  say "ok: daemon process has the input group (gid $IGID)"
else
  bad "daemon process lacks the input group (gid ${IGID:-995}) — restart via: sg input -c '... voxtype daemon ...'"
fi

# 4. X11 fallback possible: DISPLAY + XAUTHORITY in the daemon env
# environ is unreadable for sg-spawned (non-dumpable) processes — fall back
# to looking for an open X11 socket among the daemon's file descriptors.
if tr \0\ \n\ < "/proc/$DPID/environ" 2>/dev/null | grep -q ^DISPLAY=; then
  say "ok: daemon env has DISPLAY"
elif ls -l /proc/$DPID/fd 2>/dev/null | grep -q X11-unix; then
  say "ok: daemon holds an X11 connection (environ unreadable — sg spawn)"
else
  say "warn: DISPLAY unverifiable (environ unreadable, no X11 socket found) — with the input group this no longer matters for evdev"
fi

# 5. the daemon's own confession: evdev listener error at this start
if tail -40 ~/logs/voxtype.log 2>/dev/null | grep -q "No keyboard device"; then
  say "warn: log shows evdev listener error (benign only if BOTH checks 3 and 4 pass)"
fi

# 6. stale lock from a killed instance
[ -f "/run/user/$(id -u)/voxtype/voxtype.lock" ] && say "note: voxtype.lock present (fine while the daemon runs)"

if [ "$FAIL" -eq 0 ]; then say "HOTKEY PREFLIGHT: PASS"; else say "HOTKEY PREFLIGHT: FAIL — the hotkey will do nothing"; fi
exit $FAIL
