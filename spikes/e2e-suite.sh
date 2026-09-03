#!/usr/bin/env bash
# e2e-suite.sh — full end-to-end regression suite for voxtype-review.
#
# THE RULE (Sep 3, burned twice): popups spawn ONLY on the suite's own Xvfb
# display. Every spawn goes through spawn_bin() which FORCES DISPLAY=$D —
# an inherited :0 from the calling shell once covered the operator's screen
# with test windows. The cleanup kills ONLY pids this suite spawned, never a
# pattern: the operator's live popup is untouchable.
#
# Run before EVERY deploy:  ./spikes/e2e-suite.sh
# Exit 0 = safe. Exit 1 = a regression — do not ship.

set -u
cd "$(dirname "$0")/.."
BIN="$HOME/.local/bin/voxtype-review"
[[ -x "$BIN" ]] || BIN="./target/release/voxtype-review"
PASS=0; FAIL=0; SKIP=0
XVFB_PID=""; WM_PID=""
SPAWNED=()

cleanup() {
  for p in "${SPAWNED[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
  [ -n "${WM_PID:-}" ] && kill "$WM_PID" 2>/dev/null
  [ -n "${XVFB_PID:-}" ] && kill "$XVFB_PID" 2>/dev/null
  rm -f /tmp/voxtype-settings-*.pid /tmp/voxtype-settings-*.inbox 2>/dev/null
}
trap cleanup EXIT

# The ONLY sanctioned way to start the binary in this suite.
spawn_bin() {  # spawn_bin <timeout_s> [args...]  (stdin comes from the caller)
  local t=$1; shift
  env -u XAUTHORITY DISPLAY=$D timeout "$t" "$BIN" "$@" &
  SPAWNED+=($!)
}

xd() { DISPLAY=$D xdotool "$@"; }
wait_window() {  # wait_window <timeout*100ms> -> wid or empty
  local n w
  for n in $(seq 1 "$1"); do
    w=$(xd search --classname voxtype-review 2>/dev/null | tail -1)
    [ -n "$w" ] && { echo "$w"; return 0; }
    sleep 0.1
  done
  return 1
}
esc_win() { [ -n "${1:-}" ] && { DISPLAY=$D xdotool key --clearmodifiers Escape 2>/dev/null; sleep 1; }; return 0; }

ok()  { PASS=$((PASS+1)); echo "  ok: $*"; }
bad() { FAIL=$((FAIL+1)); echo "FAIL: $*"; }
skip(){ SKIP=$((SKIP+1)); echo "  skip: $*"; }

echo "== setup: isolated display =="
D=""
for n in 90 91 92 93; do [ -e "/tmp/.X11-unix/X$n" ] || { D=":$n"; break; }; done
[ -z "$D" ] && { bad "no free display in 90-93"; exit 1; }
Xvfb "$D" -screen 0 1280x800x24 -nolisten tcp >/dev/null 2>&1 & XVFB_PID=$!
for n in $(seq 1 30); do [ -e "/tmp/.X11-unix/X${D#:}" ] && break; sleep 0.1; done
if [ ! -e "/tmp/.X11-unix/X${D#:}" ]; then bad "Xvfb $D never came up"; exit 1; fi
ok "Xvfb $D up (operator display untouched)"
DISPLAY=$D xfwm4 --compositor=off >/dev/null 2>&1 & WM_PID=$!
sleep 1

echo "== t01: --no-gui passthrough =="
OUT=$(echo "passthrough check" | spawn_bin 5 --no-gui; wait $! 2>/dev/null; true)
OUT=$(echo "passthrough check" | spawn_bin 5 --no-gui; wait $! >/dev/null 2>&1; cat /tmp/.e2e-t01 2>/dev/null)
# simpler: capture via file
echo "passthrough check" | spawn_bin 5 --no-gui >/tmp/.e2e-t01 2>/dev/null
sleep 1
OUT=$(cat /tmp/.e2e-t01 2>/dev/null)
[ "$OUT" = "passthrough check" ] && ok "stdout intact" || bad "got: '$OUT'"

rm -f /tmp/voxtype-settings-*.pid /tmp/voxtype-settings-*.inbox

echo "== t02: settings window opens + pidfile written =="
echo "" | spawn_bin 90 --settings >/dev/null 2>&1
WID=$(wait_window 300)
[ -n "$WID" ] && ok "settings window mapped on $D" || bad "settings window never mapped"
PIDFILE=$(ls /tmp/voxtype-settings-*.pid 2>/dev/null | head -1)
[ -n "$PIDFILE" ] && ok "pidfile written ($(cat "$PIDFILE"))" || bad "pidfile missing"

echo "== t03: pidfile names a process that IS a settings window =="
if [ -n "$PIDFILE" ]; then
  P=$(cat "$PIDFILE")
  CMD=$(tr '\0' ' ' < "/proc/$P/cmdline" 2>/dev/null)
  case "$CMD" in
    *"--settings"*) ok "cmdline is --settings" ;;
    *) bad "pidfile pid is not a settings process: '$CMD'" ;;
  esac
else
  bad "cannot check (no pidfile)"
fi

echo "== t03b: closing the settings window removes the pidfile (Sep 3 regression) =="
# llvmpipe init is slow: give the window time to process keys before Escaping
sleep 10
# contract: Esc in the box leaves the box, second Esc (list) closes unsaved
esc_win "$WID"; sleep 0.5
esc_win "$WID"; sleep 1
[ -f "${PIDFILE:-}" ] && bad "pidfile survived the settings window" || ok "pidfile removed on graceful close"
WID=""  # closed; later tests manage their own windows

echo "== t04: stale pidfile (dead pid) does not swallow dictation =="
echo 999999 > /tmp/voxtype-settings-$(id -u).pid
printf 'dictation one\n' | spawn_bin 60 >/tmp/.e2e-out 2>/dev/null
WID2=$(wait_window 300)
[ -n "$WID2" ] && ok "normal popup opened despite stale pidfile" || bad "normal popup never opened"
esc_win "$WID2"
OUT2=$(cat /tmp/.e2e-out 2>/dev/null)
[ -z "$OUT2" ] && ok "abort emitted nothing (Esc contract)" || bad "stdout not empty: '$OUT2'"

echo "== t05: pidfile with LIVE non-settings pid does not swallow dictation =="
sleep 30 & LIVE=$!
echo $LIVE > /tmp/voxtype-settings-$(id -u).pid
printf 'dictation two\n' | spawn_bin 60 >/dev/null 2>&1
WID3=$(wait_window 300)
[ -n "$WID3" ] && ok "normal popup opened (live non-settings pid ignored)" || bad "popup swallowed by bogus pidfile"
kill $LIVE 2>/dev/null
esc_win "$WID3"

echo "== t06: handoff — settings alive, dictation goes to the inbox =="
echo "" | spawn_bin 90 --settings >/dev/null 2>&1
WID4=$(wait_window 300)
[ -n "$WID4" ] || bad "settings window for handoff never opened"
# Freeze the settings window first (SIGSTOP): determinism — it cannot eat
# the inbox before we assert on it. Thaw, then assert consumption.
SPID=$(cat "$PIDFILE" 2>/dev/null)
kill -STOP "$SPID" 2>/dev/null
printf 'gesproken zin voor de handoff\n' | spawn_bin 10 >/tmp/.e2e-out2 2>/dev/null
sleep 1
INBOX=$(ls /tmp/voxtype-settings-*.inbox 2>/dev/null | head -1)
if [ -n "$INBOX" ] && grep -q "gesproken zin" "$INBOX"; then
  ok "transcript handed off via inbox"
else
  bad "handoff inbox missing or wrong content"
fi
kill -CONT "$SPID" 2>/dev/null
OUT3=$(cat /tmp/.e2e-out2 2>/dev/null)
[ -z "$OUT3" ] && ok "handoff emitted nothing (nothing lands)" || bad "handoff stdout not empty: '$OUT3'"

echo "== t07: settings window consumes the inbox =="
sleep 2
if [ -n "${INBOX:-}" ] && [ ! -f "${INBOX:-}" ]; then
  ok "settings window consumed the inbox (file removed)"
elif [ -f "${INBOX:-/nonexistent}" ]; then
  bad "inbox not consumed: still present"
else
  skip "inbox already consumed/absent"
fi

echo "== t08: closing window2 removes ITS pidfile =="
P2=$(cat "$PIDFILE" 2>/dev/null)
esc_win "${WID4:-}"; sleep 0.5
esc_win "${WID4:-}"; sleep 1
if [ -f "${PIDFILE:-}" ] && [ "$(cat "$PIDFILE" 2>/dev/null)" = "$P2" ]; then
  bad "pidfile survived window2 (pid $P2)"
else
  ok "pidfile removed on graceful close"
fi
esc_win "$WID"; sleep 0.5; esc_win "$WID"; sleep 1  # window1 ook sluiten

echo "== t09: Esc in list aborts (empty stdout) =="
printf 'abort zin\n' | spawn_bin 60 >/tmp/.e2e-out3 2>/dev/null
WID5=$(wait_window 300)
[ -n "$WID5" ] || bad "popup for abort test never opened"
esc_win "$WID5"
OUT5=$(cat /tmp/.e2e-out3 2>/dev/null)
[ -z "$OUT5" ] && ok "abort = empty stdout" || bad "abort stdout: '$OUT5'"

echo "== RESULT: $PASS ok, $FAIL fail, $SKIP skip =="
[ "$FAIL" -eq 0 ]
