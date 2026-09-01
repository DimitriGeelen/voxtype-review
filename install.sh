#!/usr/bin/env bash
# install.sh — install voxtype-review, and optionally wire it into Voxtype.
#
# The build is the safe part. The risky part is `--wire`, which edits
# ~/.config/voxtype/config.toml — a file the operator depends on every day, and
# one where a mistake fails SILENTLY: Voxtype falls back to the raw
# transcription whenever the post_process hook errors, so a broken install looks
# exactly like no install at all.
#
# So wiring is opt-in, shows its diff, asks first, and backs up before writing.
# Everything here is reversible with --uninstall.
#
#   ./install.sh              build + install binary + starter action config
#   ./install.sh --wire       also offer the Voxtype config edit (asks first)
#   ./install.sh --check      report what is present and what is missing
#   ./install.sh --uninstall  reverse everything, restore Voxtype's config
#   ./install.sh --force      allow overwriting an existing action config
#
# Test hooks (used by spikes/m4-install.sh, so the suite never touches the real
# config): PREFIX, VOXTYPE_CONFIG, VOXTYPE_REVIEW_CONFIG_DIR, ASSUME_YES.

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

PREFIX="${PREFIX:-$HOME/.local}"
BINDIR="$PREFIX/bin"
BIN="$BINDIR/voxtype-review"
VOXTYPE_CONFIG="${VOXTYPE_CONFIG:-$HOME/.config/voxtype/config.toml}"
REVIEW_CONFIG_DIR="${VOXTYPE_REVIEW_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/voxtype-review}"
REVIEW_CONFIG="$REVIEW_CONFIG_DIR/config.toml"
BACKUP_SUFFIX="bak-voxtype-review"

# The marker lets us find our own block later without guessing, which is what
# makes both idempotence and a clean uninstall possible.
MARKER_BEGIN="# >>> voxtype-review >>>"
MARKER_END="# <<< voxtype-review <<<"

RED=$'\033[0;31m'; GREEN=$'\033[0;32m'; YELLOW=$'\033[0;33m'; BOLD=$'\033[1m'; NC=$'\033[0m'
ok()   { echo "  ${GREEN}ok${NC}    $1"; }
miss() { echo "  ${RED}--${NC}    $1"; }
warn() { echo "  ${YELLOW}!${NC}     $1"; }
info() { echo "        $1"; }
head_() { echo; echo "${BOLD}$1${NC}"; }
die()  { echo "${RED}error:${NC} $1" >&2; exit 1; }

MODE=install
FORCE=0
WIRE=0
for arg in "$@"; do
  case "$arg" in
    --check)     MODE=check ;;
    --uninstall) MODE=uninstall ;;
    --wire)      WIRE=1 ;;
    --force)     FORCE=1 ;;
    -h|--help)
      sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) die "unknown option: $arg (try --help)" ;;
  esac
done

# The exact block written into Voxtype's config. Kept in one place so that what
# --wire writes, what --check looks for and what the README documents cannot
# drift apart.
wire_block() {
  cat <<EOF
$MARKER_BEGIN
# Added by voxtype-review's install.sh. Remove this block (or run
# ./install.sh --uninstall) to go back to plain dictation.
[output.post_process]
command = "$BIN"
# Ten minutes. This is a human reading their own words, not a model answering.
# Voxtype kills the hook at this timeout and falls back to the raw
# transcription, so a short value silently discards edits.
timeout_ms = 600000
# Voxtype trims surrounding whitespace by default; leave it to the operator.
trim = false
# Anything goes wrong -> the original transcription, never nothing.
fallback_on_empty = true
$MARKER_END
EOF
}

is_wired() {
  [ -f "$VOXTYPE_CONFIG" ] && grep -qF "$MARKER_BEGIN" "$VOXTYPE_CONFIG"
}

# An uncommented [output.post_process] that is not ours. Voxtype would take the
# first one; silently adding a second is how you get a working install that does
# nothing. Comment lines do not count — the shipped config has the block
# commented out as documentation.
has_foreign_post_process() {
  [ -f "$VOXTYPE_CONFIG" ] || return 1
  if is_wired; then
    # Strip our block, then look at what remains.
    sed "/^${MARKER_BEGIN}\$/,/^${MARKER_END}\$/d" "$VOXTYPE_CONFIG" \
      | grep -qE '^\s*\[output\.post_process\]'
  else
    grep -qE '^\s*\[output\.post_process\]' "$VOXTYPE_CONFIG"
  fi
}

confirm() {
  local prompt="$1"
  if [ "${ASSUME_YES:-0}" = "1" ]; then
    info "(ASSUME_YES=1 — proceeding without asking)"
    return 0
  fi
  if [ ! -t 0 ]; then
    warn "not a terminal and ASSUME_YES is not set — refusing to guess."
    return 1
  fi
  local reply
  printf '%s [type yes to confirm]: ' "$prompt"
  read -r reply
  [ "$reply" = "yes" ]
}

check_prereq() {
  local found=0
  head_ "Prerequisites"

  if command -v cargo >/dev/null; then ok "cargo $(cargo --version 2>/dev/null | awk '{print $2}')"
  else miss "cargo — needed to build. https://rustup.rs"; found=1; fi

  if command -v xdotool >/dev/null; then ok "xdotool"
  else miss "xdotool — needed for focus capture/restore. apt install xdotool"; found=1; fi

  if command -v voxtype >/dev/null; then ok "voxtype $(voxtype --version 2>/dev/null | awk '{print $2}')"
  else miss "voxtype — this is a hook for Voxtype. https://voxtype.org"; found=1; fi

  local st="${XDG_SESSION_TYPE:-unknown}"
  if [ "$st" = "x11" ]; then
    ok "X11 session"
  elif [ -n "${DISPLAY:-}" ] && [ "$st" = "unknown" ]; then
    ok "X display present ($DISPLAY)"
  else
    miss "session type is '$st' — this needs X11. Focus restore has no portable"
    info "Wayland equivalent, so your text would land in the wrong window."
    found=1
  fi

  # Advisory only: the default actions need it, the tool does not.
  if command -v ollama >/dev/null; then
    if ollama list >/dev/null 2>&1; then ok "ollama (default actions will work)"
    else warn "ollama installed but not responding — default actions will fall back"; fi
  else
    warn "ollama absent — the DEFAULT actions need it, but editing and accepting"
    info "do not. Point actions at any command you like; see the README."
  fi

  # Same advisory level as ollama, and for the same reason: the tool runs
  # without them, the shipped actions do not. They are checked separately
  # because their failure mode is worse than ollama's is. Since T-015 the
  # default commands are `jq -Rs … | curl … | jq -r`, and run_action discards
  # the child's stderr (src/core.rs), so `jq: command not found` goes nowhere at
  # all: the action returns the transcript unchanged and the operator concludes
  # the model ignored them. Naming the symptom here is the only place this gets
  # said out loud.
  local lacking=""
  command -v jq   >/dev/null || lacking="jq"
  command -v curl >/dev/null || lacking="${lacking:+$lacking }curl"
  if [ -z "$lacking" ]; then
    ok "jq and curl (the default actions reach ollama over HTTP)"
  else
    warn "$lacking absent — every DEFAULT action pipes through both, and the"
    info "failure is SILENT: the action hands back your transcript unchanged,"
    info "with no error on any surface. It looks like the model ignored you."
    info "apt install $lacking"
  fi

  return $found
}

do_check() {
  local incomplete=0
  check_prereq || incomplete=1

  head_ "Install"
  if [ -x "$BIN" ]; then ok "binary: $BIN"
  else miss "binary not installed at $BIN"; incomplete=1; fi

  case ":$PATH:" in
    *":$BINDIR:"*) ok "$BINDIR is on PATH" ;;
    *) warn "$BINDIR is NOT on PATH — Voxtype is wired with an absolute path so"
       info "this still works, but 'voxtype-review --list-actions' will not." ;;
  esac

  if [ -f "$REVIEW_CONFIG" ]; then
    ok "action config: $REVIEW_CONFIG"
    if [ -x "$BIN" ]; then
      local n
      n=$("$BIN" --list-actions 2>/dev/null | grep -cE '^[0-9]  ' || true)
      info "${n:-0} action(s) load from it"
    fi
  else
    warn "no action config — built-in defaults will be used"
  fi

  head_ "Voxtype wiring"
  if [ ! -f "$VOXTYPE_CONFIG" ]; then
    miss "no Voxtype config at $VOXTYPE_CONFIG"
    incomplete=1
  elif is_wired; then
    ok "wired into $VOXTYPE_CONFIG"
    local cmd
    cmd=$(sed -n "/^${MARKER_BEGIN}\$/,/^${MARKER_END}\$/p" "$VOXTYPE_CONFIG" \
          | grep -E '^command = ' | head -1 | cut -d'"' -f2)
    if [ "$cmd" != "$BIN" ]; then
      warn "wired command is '$cmd' but the binary is at '$BIN'"
      incomplete=1
    fi
  else
    miss "not wired — run ./install.sh --wire"
    incomplete=1
  fi

  if has_foreign_post_process; then
    warn "another [output.post_process] exists in the config. Voxtype uses the"
    info "first one it finds, so only one of them is doing anything."
  fi

  echo
  if [ "$incomplete" -eq 0 ]; then
    echo "${GREEN}Install looks complete.${NC}"
  else
    echo "${YELLOW}Install is incomplete — see the '--' lines above.${NC}"
  fi
  return $incomplete
}

do_build() {
  head_ "Building"
  command -v cargo >/dev/null || die "cargo not found — install Rust from https://rustup.rs"
  ( cd "$HERE" && cargo build --release ) || die "build failed (output above)"
  [ -x "$HERE/target/release/voxtype-review" ] || die "build reported success but no binary at target/release/"
  ok "built $HERE/target/release/voxtype-review"

  mkdir -p "$BINDIR" || die "cannot create $BINDIR"
  install -m 0755 "$HERE/target/release/voxtype-review" "$BIN" || die "cannot install to $BIN"
  ok "installed $BIN"
}

do_config() {
  head_ "Action config"
  mkdir -p "$REVIEW_CONFIG_DIR" || die "cannot create $REVIEW_CONFIG_DIR"
  if [ -f "$REVIEW_CONFIG" ] && [ "$FORCE" -eq 0 ]; then
    ok "keeping your existing $REVIEW_CONFIG"
    info "(--force would overwrite it with the starter file)"
    return 0
  fi
  local args=(--write-default-config)
  [ "$FORCE" -eq 1 ] && args+=(--force)
  if VOXTYPE_REVIEW_CONFIG="$REVIEW_CONFIG" "$BIN" "${args[@]}" >/dev/null 2>&1; then
    ok "wrote starter config to $REVIEW_CONFIG"
  else
    die "could not write $REVIEW_CONFIG"
  fi
}

do_wire() {
  head_ "Wiring into Voxtype"

  [ -f "$VOXTYPE_CONFIG" ] || die "no Voxtype config at $VOXTYPE_CONFIG (is Voxtype installed?)"

  if is_wired; then
    ok "already wired — nothing to do"
    return 0
  fi

  if has_foreign_post_process; then
    warn "your config already has an [output.post_process] block."
    info "Voxtype uses the first one, so adding ours would leave one of them"
    info "doing nothing at all — silently. Comment out or remove the existing"
    info "block first, then run --wire again."
    return 1
  fi

  echo "  This will append the following to ${BOLD}$VOXTYPE_CONFIG${NC}:"
  echo
  wire_block | sed 's/^/      /'
  echo
  info "A backup will be written to $(basename "$VOXTYPE_CONFIG").$BACKUP_SUFFIX first."
  info "Undo at any time with: ./install.sh --uninstall"
  echo

  if ! confirm "  Append this block?"; then
    warn "not wired — nothing was changed."
    return 1
  fi

  cp -p "$VOXTYPE_CONFIG" "$VOXTYPE_CONFIG.$BACKUP_SUFFIX" || die "backup failed — refusing to edit"
  ok "backed up to $VOXTYPE_CONFIG.$BACKUP_SUFFIX"

  { printf '\n'; wire_block; } >> "$VOXTYPE_CONFIG" || die "append failed"
  ok "wired"
  echo
  echo "  ${BOLD}Restart Voxtype for this to take effect.${NC}"
}

do_uninstall() {
  head_ "Uninstalling"

  if is_wired; then
    cp -p "$VOXTYPE_CONFIG" "$VOXTYPE_CONFIG.$BACKUP_SUFFIX-preuninstall" 2>/dev/null
    # Remove only our marked block. Anything the operator added by hand is
    # theirs and stays.
    local tmp="$VOXTYPE_CONFIG.tmp.$$"
    if sed "/^${MARKER_BEGIN}\$/,/^${MARKER_END}\$/d" "$VOXTYPE_CONFIG" > "$tmp"; then
      mv "$tmp" "$VOXTYPE_CONFIG" && ok "removed our block from $VOXTYPE_CONFIG"
    else
      rm -f "$tmp"
      warn "could not edit $VOXTYPE_CONFIG — remove the voxtype-review block by hand"
    fi
    info "restart Voxtype to go back to plain dictation"
  else
    ok "Voxtype config was not wired — left alone"
  fi

  if [ -e "$BIN" ]; then
    rm -f "$BIN" && ok "removed $BIN"
  else
    ok "no binary at $BIN"
  fi

  # The action config is the operator's work, not ours. Deleting it would throw
  # away whatever they wrote. Say where it is and leave it.
  if [ -f "$REVIEW_CONFIG" ]; then
    warn "left your action config alone: $REVIEW_CONFIG"
    info "(it is yours — delete it by hand if you want it gone)"
  fi

  echo
  echo "${GREEN}Uninstalled.${NC}"
}

case "$MODE" in
  check)
    do_check
    exit $?
    ;;
  uninstall)
    do_uninstall
    exit 0
    ;;
  install)
    check_prereq || {
      echo
      warn "some prerequisites are missing (see above)."
      warn "continuing with the build — --check will tell you what still needs doing."
    }
    do_build
    do_config
    if [ "$WIRE" -eq 1 ]; then
      do_wire || true
    else
      head_ "Voxtype wiring"
      info "not wired — the binary is installed but Voxtype does not call it yet."
      info "Run ./install.sh --wire to be shown the exact change and asked."
    fi

    head_ "Next"
    echo "  $BIN --list-actions        # what loaded, and from where"
    echo "  \$EDITOR $REVIEW_CONFIG"
    [ "$WIRE" -eq 1 ] || echo "  ./install.sh --wire        # hook it into Voxtype"
    echo "  ./install.sh --check       # verify the whole install"
    echo
    ;;
esac
