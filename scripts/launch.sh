#!/usr/bin/env bash
# launch.sh — interactive `pillbox run` launcher, with saved profiles.
#
# Interactive mode walks you through the options (agent, backend, vault,
# memory, telemetry, seeded prompt), preflight-checks everything the chosen
# combination needs (binary built+codesigned, runner image, Docker, Raindrop,
# kypp, agent auth), explains how to fix whatever's missing, then prints the
# full composed command before running it — so the flags stop being folklore.
# At the end it offers to save the choices as a named profile.
#
#   scripts/launch.sh                     interactive picker (+ optional save)
#   scripts/launch.sh <profile>           replay a profile (interactive agent session)
#   scripts/launch.sh <profile> "<task>"  replay with a seeded prompt
#   scripts/launch.sh --list              list saved profiles
#
# Profiles live in ~/.pillbox/launch/<name>.profile as plain KEY=VALUE lines
# (parsed, never sourced — a config file can't execute code). The seed prompt
# is intentionally NOT saved: it's per-run, pass it as the second argument.
# Replay still runs the full preflight — that's the point of the launcher
# over a shell alias.
#
# Env overrides: PILLBOX (binary path), PILLBOX_RUNNER_IMAGE (image tag),
# OTEL endpoint defaults to Raindrop Workshop (http://localhost:5899).
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PB="${PILLBOX:-$ROOT/target/debug/pillbox}"
IMAGE="${PILLBOX_RUNNER_IMAGE:-pillbox-runner:dev}"
OTEL_ENDPOINT="${OTEL_EXPORTER_OTLP_ENDPOINT:-http://localhost:5899}"
PROFILE_DIR="$HOME/.pillbox/launch"

die()  { echo "launch: $1" >&2; exit 1; }

# Every choice (and the final run confirmation) reads from the tty. Without
# one, `read </dev/tty` fails silently and each prompt falls through to its
# default — which culminates in actually launching a VM. Refuse instead.
# (--list/--help are fine without a terminal; checked after arg parse below.)
require_tty() {
  { : </dev/tty; } 2>/dev/null || die "interactive — run it from a terminal"
}

bold() { printf '\033[1m%s\033[0m' "$1"; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$1"; }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$1"; }
note() { printf '    \033[2m%s\033[0m\n' "$1"; }

# ── profiles ─────────────────────────────────────────────────────────────
list_profiles() {
  local found=0 f
  for f in "$PROFILE_DIR"/*.profile; do
    [ -e "$f" ] || break # bash 3.2 has no nullglob default: an unmatched glob iterates once as the literal pattern
    found=1
    local n; n="$(basename "$f" .profile)"
    printf '  %-14s %s\n' "$n" "$(profile_summary "$f")"
  done
  [ "$found" -eq 1 ] || echo "  (none — run scripts/launch.sh and save one)"
}

profile_get() { sed -n "s/^$2=//p" "$1" | head -1; }

profile_summary() {
  local agent backend flags=""
  agent="$(profile_get "$1" AGENT)"; backend="$(profile_get "$1" BACKEND)"
  [ -n "$(profile_get "$1" VAULT)" ] && flags="$flags --vault"
  [ -n "$(profile_get "$1" MEMORY)" ] && flags="$flags --memory"
  [ -n "$(profile_get "$1" TELEMETRY)" ] && flags="$flags +otel"
  echo "$agent/$backend$flags"
}

load_profile() { # sets the GLOBAL choice vars (AGENT/BACKEND/...) from a profile file
  local f="$1"
  AGENT="$(profile_get "$f" AGENT)"
  BACKEND="$(profile_get "$f" BACKEND)"
  VAULT="$(profile_get "$f" VAULT)"
  MEMORY="$(profile_get "$f" MEMORY)"
  TELEMETRY="$(profile_get "$f" TELEMETRY)"
  local img; img="$(profile_get "$f" IMAGE)"
  [ -n "$img" ] && IMAGE="$img"
  [ -n "$AGENT" ] && [ -n "$BACKEND" ] || die "profile $(basename "$f") is missing AGENT/BACKEND"
}

save_profile_offer() {
  local name
  echo
  read -r -p "$(bold "Save these choices as a profile?") (name, empty to skip): " name </dev/tty
  [ -n "$name" ] || return 0
  [[ "$name" =~ ^[a-zA-Z0-9_-]+$ ]] || { echo "  skipped: name must be alphanumeric/_/- only" >&2; return 0; }
  mkdir -p "$PROFILE_DIR"
  cat > "$PROFILE_DIR/$name.profile" <<EOF
AGENT=$AGENT
BACKEND=$BACKEND
VAULT=$VAULT
MEMORY=$MEMORY
TELEMETRY=$TELEMETRY
IMAGE=$IMAGE
EOF
  echo "  saved — replay with: scripts/launch.sh $name [\"task\"]"
}

# ── pickers ──────────────────────────────────────────────────────────────
# pick "Question" default opt1 "explanation1" opt2 "explanation2" ...
# Prints the chosen option name on stdout (captured by $(...)), so the menu
# goes to stderr and input is read from the tty, not stdin. A non-numeric
# answer is taken as a literal option name (you can type "codex" instead of 2).
pick() {
  local q="$1" def="$2"; shift 2
  local opts=() descs=()
  while [ $# -gt 0 ]; do opts+=("$1"); descs+=("$2"); shift 2; done
  echo >&2; printf '%s\n' "$(bold "$q")" >&2
  local i
  for i in "${!opts[@]}"; do
    local mark=" "
    [ "${opts[$i]}" = "$def" ] && mark="*"
    printf ' %s %d) %-12s %s\n' "$mark" $((i + 1)) "${opts[$i]}" "${descs[$i]}" >&2
  done
  local choice
  read -r -p "  choice [default ${def}]: " choice </dev/tty
  if [ -z "$choice" ]; then echo "$def"; return; fi
  if [[ "$choice" =~ ^[0-9]+$ ]] && [ "$choice" -ge 1 ] && [ "$choice" -le "${#opts[@]}" ]; then
    echo "${opts[$((choice - 1))]}"
  else
    echo "$choice"
  fi
}

yesno() { # yesno "Question" default(y|n) "why you'd want it"
  local q="$1" def="$2" why="$3" ans
  echo >&2; printf '%s  \033[2m(%s)\033[0m\n' "$(bold "$q")" "$why" >&2
  read -r -p "  y/n [default ${def}]: " ans </dev/tty
  ans="${ans:-$def}"
  [ "$ans" = "y" ] || [ "$ans" = "Y" ]
}

# ── mode: list / profile replay / interactive ────────────────────────────
INTERACTIVE=1
PROMPT=""
case "${1:-}" in
  --list|-l)
    printf '%s\n' "$(bold "Saved profiles") ($PROFILE_DIR)"
    list_profiles
    exit 0
    ;;
  -h|--help)
    sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
  ?*)
    # NOTE: no ${1@Q} here — bash 3.2 (macOS) aborts on it ("bad substitution")
    # *before* the exit, and the script would fall through to interactive mode.
    [[ "$1" =~ ^[a-zA-Z0-9_-]+$ ]] || die "unknown option '$1' (profiles are alphanumeric/_/-)"
    if [ ! -f "$PROFILE_DIR/$1.profile" ]; then
      echo "launch: no profile named '$1'. Saved profiles:" >&2
      list_profiles >&2
      exit 1
    fi
    load_profile "$PROFILE_DIR/$1.profile"
    PROMPT="${2:-}"
    INTERACTIVE=0
    echo "$(bold "Profile") $1: $(profile_summary "$PROFILE_DIR/$1.profile")"
    ;;
esac
require_tty

if [ "$INTERACTIVE" -eq 1 ]; then
  AGENT="$(pick "Agent?" claude \
    claude   "Claude Code (PTY, vault-capable, interactive or seeded)" \
    codex    "OpenAI Codex (PTY, vault-capable)" \
    opencode "opencode serve (server-mode: drive via session send, no terminal)" \
    pi       "pi coding agent (PTY)")"

  BACKEND="$(pick "Backend?" libkrun \
    libkrun "microVM (HVF) — own egress fence + in-child MITM vault; the dogfood path" \
    docker  "container — cross-platform fallback; vault runs host-side")"

  VAULT=""
  if yesno "Vault (credential stub-swap MITM)?" y \
    "agent sees a stub credential; the real one is swapped in on the wire — on libkrun OAuth creds are always env-forked, the flag adds the --with API-key swap + parity"; then
    VAULT=1
  fi

  MEMORY=""
  if yesno "Memory (kypp brief + capture)?" y \
    "briefs project memory into a seeded prompt at start, distills the trajectory back after the run; needs kypp on PATH"; then
    MEMORY=1
  fi

  TELEMETRY=""
  if yesno "Telemetry (OTLP -> Raindrop Workshop)?" y \
    "session + gen_ai spans land in Workshop at ${OTEL_ENDPOINT}; needs Raindrop running"; then
    TELEMETRY=1
  fi

  echo
  read -r -p "$(bold "Seed prompt") (empty = interactive session): " PROMPT </dev/tty
fi

# ── preflight ────────────────────────────────────────────────────────────
echo; printf '%s\n' "$(bold "Preflight")"
fail=0

if [ -x "$PB" ]; then
  ok "pillbox binary: $PB"
else
  bad "no pillbox binary at $PB"
  note "build it: scripts/lk-build.sh (libkrun) or cargo build (docker-only)"
  fail=1
fi

if [ "$BACKEND" = libkrun ] && [ -x "$PB" ]; then
  # nm + codesign: a bare cargo build strips the HVF entitlement and pillbox
  # silently falls back to docker — catch it here instead.
  if [ "$(nm "$PB" 2>/dev/null | grep -c LibkrunBackend)" -ge 1 ]; then
    ok "binary has the libkrun backend"
  else
    bad "binary lacks libkrun"; note "rebuild: scripts/lk-build.sh"; fail=1
  fi
  if [ "$(codesign -d --entitlements :- "$PB" 2>/dev/null | grep -c hypervisor)" -ge 1 ]; then
    ok "binary codesigned with the HVF entitlement"
  else
    bad "binary not codesigned — libkrun would silently fall back to docker"
    note "fix: scripts/lk-build.sh"
    fail=1
  fi
fi

if docker info >/dev/null 2>&1; then
  ok "docker daemon up"
  if docker image inspect "$IMAGE" >/dev/null 2>&1; then
    ok "runner image: $IMAGE"
  else
    bad "runner image '$IMAGE' not found"
    note "build: docker buildx build -f runner/Dockerfile -t $IMAGE ."
    fail=1
  fi
else
  bad "docker daemon unreachable (libkrun needs it too — rootfs comes from docker export)"
  fail=1
fi

if [ -n "$TELEMETRY" ]; then
  if curl -s --max-time 2 -o /dev/null "$OTEL_ENDPOINT"; then
    ok "raindrop reachable at $OTEL_ENDPOINT"
  else
    bad "nothing answering at $OTEL_ENDPOINT — spans would be dropped"
    note "start Raindrop Workshop, or rerun without telemetry"
    fail=1
  fi
fi

if [ -n "$MEMORY" ]; then
  if command -v kypp >/dev/null; then
    ok "kypp on PATH ($(command -v kypp))"
  else
    bad "kypp not on PATH — --memory would silently no-op"
    note "install: uv tool install ~/code/kypp"
    fail=1
  fi
fi

if [ -x "$PB" ]; then
  AUTH_ID="$AGENT"; [ "$AGENT" = codex-serve ] && AUTH_ID=codex
  if "$PB" auth list --json 2>/dev/null \
    | python3 -c 'import json,sys; a=json.load(sys.stdin)["agents"]; sys.exit(0 if any(x["id"]==sys.argv[1] and x["authenticated"] for x in a) else 1)' "$AUTH_ID" 2>/dev/null; then
    ok "agent auth present for $AUTH_ID"
  else
    bad "no stored credentials for $AUTH_ID"
    note "login: $PB auth login --agent $AUTH_ID"
    fail=1
  fi
fi

if [ "$fail" -ne 0 ]; then
  echo; echo "preflight failed — fix the ✗ items above and rerun." >&2
  exit 1
fi

# ── compose + run ────────────────────────────────────────────────────────
envs=()
[ "$BACKEND" = libkrun ] && envs+=("PILLBOX_BACKEND=libkrun")
envs+=("PILLBOX_RUNNER_IMAGE=$IMAGE")
[ -n "$TELEMETRY" ] && envs+=("OTEL_EXPORTER_OTLP_ENDPOINT=$OTEL_ENDPOINT")

cmd=("$PB" run --agent "$AGENT")
[ -n "$VAULT" ] && cmd+=(--vault)
[ -n "$MEMORY" ] && cmd+=(--memory)
[ -n "$PROMPT" ] && cmd+=(-- "$PROMPT")

echo; printf '%s\n' "$(bold "Command")"
printf '  %s \\\n' "${envs[@]}"
printf '  %q ' "${cmd[@]}"; echo; echo

[ "$INTERACTIVE" -eq 1 ] && save_profile_offer

read -r -p "run it? [Y/n] " go </dev/tty
[ "${go:-y}" = y ] || [ "${go:-y}" = Y ] || exit 0

exec env "${envs[@]}" "${cmd[@]}"
