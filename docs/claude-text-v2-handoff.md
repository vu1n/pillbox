# Claude text v2 validation and Mac handoff

This change adds `claude_code` to `pillbox.text/2` on the invocation-owned libkrun
backend. Its request names only the harness, model and effort. Pillbox resolves
the image, catalog, existing vault credential and provider egress internally;
native `system/init` supplies the observed Claude version. No other driver changes.

The isolated branch is `codex/text-v2-claude`, based on open PR180
(`claude/project-thread-zucgk7`) at
`354428cc5cda503c078e6167b797271e22b604b9`. The draft PR must remain blocked on
the live Mac gate below. No live provider turn was launched in cloud-dev.

## Shared integration footprint

The Claude parser, executor, guest launcher and supervisor are new modules.
The common changes are:

| File | Change |
|---|---|
| `src/execution/mod.rs` | Register the Claude text module for libkrun and pure Linux tests. |
| `src/execution/text_v2.rs` | Dispatch Claude selections; validate Claude admission limits and reject unsupported models before image/credential lookup. The existing Codex path is retained. |
| `src/execution/text.rs` | Expose the existing stage recorder and cancellation/deadline check to sibling text drivers. Visibility changes only. |
| `src/sandbox/libkrun/repository.rs` | Register the Claude launcher and add console-only draining for a one-shot guest whose terminal RPC bytes remain queued after exit. Existing callers retain their liveness checks. |
| `src/vault/refresh.rs`, `src/vault/mod.rs` | Add `pre_refresh_until(path, auth_id, deadline)`. Its lock wait and single HTTP grant share the invocation budget. Existing `pre_refresh` callers retain their 35-second lock and 30-second HTTP limits and rotation semantics. |
| `.github/workflows/ci.yml` | Run the Claude supervisor fixtures and live-script syntax check offline on Linux. |
| `.brief/SIGNOFF` | Record conformance of the bounded host refresh to the existing OAuth broker decision. |

`src/execution/usage.rs` is unchanged: Claude's `result.total_cost_usd` and
non-overlapping input/output/cache counts already have the required mapping.
The adapter calls that parser before checking result policy and limits, preserving
reported spend on a rejected result. Usage is omitted when nothing valid was reported.

For integration, inspect only the common diff with:

```sh
git diff 354428cc5cda503c078e6167b797271e22b604b9 HEAD -- \
  src/execution/mod.rs src/execution/text.rs src/execution/text_v2.rs \
  src/sandbox/libkrun/repository.rs src/vault/mod.rs src/vault/refresh.rs \
  .github/workflows/ci.yml .brief/SIGNOFF
```

## Cloud validation

The actual `$ship-it` instructions were read from
`/workspace/dev/codex-skills/ship-it/SKILL.md`; no repository-local copy exists.
Correctness and security reviews ran independently, including a separate attempt
to refute the security finding. Confirmed findings were fixed before publication:

- Drain queued terminal bytes after a one-shot guest exits.
- Require native `stop_reason: end_turn` and `terminal_reason: completed`; reject
  missing, malformed, truncated, empty, oversized or duplicate finals.
- Keep Claude admission restrictions scoped to Claude and exclude older models
  whose CLI silently drops requested effort.
- Bound credential lock wait and the whole refresh response within the remaining
  invocation deadline, while retaining at-most-once grant handling.
- Prefix raw native lines with a fixed transport byte and emit exit proof only
  from the trusted supervisor after waiting for the child. Native JSON cannot
  forge exit success. Account for evidence envelopes and diagnostics in bounds.

The final reviews reported no remaining material correctness or security finding.
The local cleanup pass retained explicit protocol guards, simplified the launch
input and avoided a duplicate result parser in the guest supervisor.

Exact validation commands are below. In cloud-dev, each `cargo` invocation was
prefixed with `CARGO_HOME=/workspace/pillbox-toolchain/cargo
RUSTUP_HOME=/workspace/pillbox-toolchain/rustup` and used
`/workspace/pillbox-toolchain/cargo/bin/cargo` (stable Rust 1.99.0).

```sh
UV_CACHE_DIR=/tmp/pillbox-uv-cache UV_TOOL_DIR=/tmp/pillbox-uv-tools \
  uvx --from git+https://github.com/vu1n/brief@v0.4.2 brief pin
UV_CACHE_DIR=/tmp/pillbox-uv-cache UV_TOOL_DIR=/tmp/pillbox-uv-tools \
  uvx --from git+https://github.com/vu1n/brief@v0.4.2 brief check
UV_CACHE_DIR=/tmp/pillbox-uv-cache UV_TOOL_DIR=/tmp/pillbox-uv-tools \
  uvx --from git+https://github.com/vu1n/brief@v0.4.2 brief check \
  --base origin/claude/project-thread-zucgk7
UV_CACHE_DIR=/tmp/pillbox-uv-cache UV_TOOL_DIR=/tmp/pillbox-uv-tools \
  uvx --from git+https://github.com/vu1n/brief@v0.4.2 brief doctor
cargo fmt --check
cargo clippy --no-default-features --all-targets -- -D warnings
cargo test --no-default-features --all-targets
cargo check --features libkrun --all-targets
bash -n scripts/smoke/claude-text.sh
python3 scripts/smoke/test_claude_text_bridge.py
python3 scripts/smoke/claude-text-offline.py --claude /tmp/pillbox-claude-native/package/claude
git diff --check
```

Pure fake-harness tests cover resolved metadata and usage, unsupported selection,
missing credentials without configuring access, all tool classes including MCP,
permission denials, final-text limits, truncation, framing, forged exit proof,
deadline and cancellation. The Python supervisor suite has three passing tests.
The offline test used the actual Claude Code 2.1.289 binary against a loopback
Anthropic SSE mock with a placeholder API key: both text and injected Bash cases
advertised empty tools/MCP and executed no tool. It used no vault or provider.
The live script was also exercised with a temporary fake Pillbox/evidence fixture:
valid evidence passes and a native tool event fails.

The final Linux all-target suite passed with 880 tests passed, zero failed and
four ignored (829 unit and 51 integration tests). Format, strict
no-default-features clippy, and the committed PR-range governance check passed.

Linux libkrun compile-only checking passes. Additional strict Linux feature lint
(`cargo clippy --features libkrun --all-targets -- -D warnings`) fails on the
pre-existing unused `bail` import in `src/sandbox/libkrun/rootfs_backing.rs:9`;
that import is used on macOS. The required Linux no-default-features lint passes.
The dependency `binrw` emits a pre-existing future-incompatibility advisory.
Cloud Linux cannot prove linked macOS behavior or a real VM/provider turn.

## Required live macOS gate

The parent must coordinate a Mac with the existing libkrun toolchain, runner image
and configured vault subscription OAuth. Do not create credentials, run sign-in,
change access, choose a Docker execution backend or substitute an API key. Do not
launch the final command until the parent has obtained any required paid-turn
approval. The test sends one short prompt on `claude-opus-4-8` with low effort.

Fetch the published branch into an isolated checkout and verify its HEAD equals
the final remote SHA in the handoff. From that checkout, run:

```sh
git fetch origin codex/text-v2-claude
git worktree add ../pillbox-claude-live --detach FETCH_HEAD
cd ../pillbox-claude-live
git rev-parse HEAD
cargo fmt --check
cargo clippy --all-targets --features libkrun -- -D warnings
cargo test --all-targets --features libkrun
cargo build --features libkrun
codesign -f --entitlements krun/entitlements.plist -s - target/debug/pillbox
```

Then, only after the live-turn authorization:

```sh
evidence_dir="$(mktemp -d "${TMPDIR:-/tmp}/pillbox-claude-live.XXXXXX")"
/usr/bin/time -p bash scripts/smoke/claude-text.sh --live "$PWD/target/debug/pillbox" "$evidence_dir"
```

The script uses the configured runner image and existing credential store. Its
request carries no image, version, credential reference or egress override.
It requires exactly `PILLBOX_CLAUDE_TEXT_OK`, a completed record, all six resolved
fields, at most 32 KiB of final text, one native success with empty
`permission_denials`, valid stop/completion reasons, and no tool events in native
evidence or the session log. Retain the private evidence directory and report the
exact commit, observed harness version, image digest, requested/served models,
reported usage, exit status and wall time. A failure is a gate failure to fix,
not permission to weaken the assertions.

Hosted CI is a separate gate. If Actions cannot start because repository minutes
are exhausted, report the unstarted jobs and platform reason separately from test
failures. Do not bypass protection or merge this draft while a required gate is open.
