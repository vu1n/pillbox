#!/usr/bin/env bash
# Exercise the production ManagedBackend admission boundary with Codex selected.
# The isolated state directory and loopback endpoint turn every downstream side
# effect into an observable failure: the only passing result is the first-line
# unsupported_execution rejection with no state or socket activity.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
if [[ -n "${PILLBOX_BIN:-}" ]]; then
  pillbox_bin="$PILLBOX_BIN"
else
  pillbox_bin="$repo_root/target/debug/pillbox"
  cargo build --quiet --manifest-path "$repo_root/Cargo.toml" --bin pillbox
fi
if [[ ! -x "$pillbox_bin" ]]; then
  echo "managed agent preflight: pillbox binary is not executable: $pillbox_bin" >&2
  exit 1
fi

smoke_root="$(mktemp -d "${TMPDIR:-/tmp}/pillbox-managed-preflight.XXXXXX")"
smoke_home="$smoke_root/home"
smoke_workspace="$smoke_root/workspace"
socket_log="$smoke_root/socket.log"
port_file="$smoke_root/port"
stderr_file="$smoke_root/stderr"
mkdir -p "$smoke_home" "$smoke_workspace"
: >"$socket_log"

listener_pid=""
cleanup() {
  if [[ -n "$listener_pid" ]]; then
    kill "$listener_pid" 2>/dev/null || true
    wait "$listener_pid" 2>/dev/null || true
  fi
  rm -rf "$smoke_root"
}
trap cleanup EXIT

node -e '
  const fs = require("node:fs");
  const net = require("node:net");
  const log = process.argv[1];
  const server = net.createServer((socket) => {
    fs.appendFileSync(log, "connection\n");
    socket.destroy();
  });
  server.listen(0, "127.0.0.1", () => process.stdout.write(`${server.address().port}\n`));
' "$socket_log" >"$port_file" &
listener_pid=$!

for _ in $(seq 1 100); do
  [[ -s "$port_file" ]] && break
  sleep 0.01
done
if [[ ! -s "$port_file" ]]; then
  echo "managed agent preflight: loopback observer did not start" >&2
  exit 1
fi
port="$(tr -d '[:space:]' <"$port_file")"

state_before="$(find "$smoke_home" "$smoke_workspace" -mindepth 1 -print | wc -l | tr -d '[:space:]')"
set +e
HOME="$smoke_home" \
PILLBOX_BACKEND=managed \
PILLBOX_MANAGED_URL="https://127.0.0.1:$port" \
PILLBOX_MANAGED_TOKEN_SECRET=must-not-be-read \
PILLBOX_R2_CF_API_TOKEN=must-not-be-read \
PILLBOX_EVENTS_WEBHOOK= \
OTEL_EXPORTER_OTLP_ENDPOINT= \
HTTP_PROXY= HTTPS_PROXY= ALL_PROXY= NO_PROXY=127.0.0.1 \
"$pillbox_bin" run --agent codex --workspace "$smoke_workspace" -- must-not-run \
  > /dev/null 2>"$stderr_file"
exit_code=$?
set -e

sleep 0.05
network_requests="$(wc -l <"$socket_log" | tr -d '[:space:]')"
state_after="$(find "$smoke_home" "$smoke_workspace" -mindepth 1 -print | wc -l | tr -d '[:space:]')"
state_entries_created=$((state_after - state_before))
observed_output="$(<"$stderr_file")"

if [[ "$exit_code" -ne 2 ]] ||
   [[ "$observed_output" != *"unsupported_execution"* ]] ||
   [[ "$network_requests" -ne 0 ]] ||
   [[ "$state_entries_created" -ne 0 ]]; then
  echo "managed agent preflight: production boundary did not fail closed" >&2
  echo "exit_code=$exit_code network_requests=$network_requests state_entries_created=$state_entries_created" >&2
  printf '%s\n' "$observed_output" >&2
  exit 1
fi

PREFLIGHT_OUTPUT="$observed_output" \
PREFLIGHT_EXIT_CODE="$exit_code" \
PREFLIGHT_NETWORK_REQUESTS="$network_requests" \
PREFLIGHT_STATE_ENTRIES_CREATED="$state_entries_created" \
node -e '
  const { createHash } = require("node:crypto");
  const output = process.env.PREFLIGHT_OUTPUT;
  process.stdout.write(`${JSON.stringify({
    schema_version: 1,
    agent: "codex",
    status: "preflight_rejected",
    disposition: "not_sent",
    error_code: "unsupported_execution",
    exit_code: Number(process.env.PREFLIGHT_EXIT_CODE),
    observed_output: output,
    observed_output_sha256: `sha256:${createHash("sha256").update(output).digest("hex")}`,
    counters: {
      provision_attempts: Number(process.env.PREFLIGHT_NETWORK_REQUESTS),
      network_requests: Number(process.env.PREFLIGHT_NETWORK_REQUESTS),
      state_entries_created: Number(process.env.PREFLIGHT_STATE_ENTRIES_CREATED),
    },
  })}\n`);
'
