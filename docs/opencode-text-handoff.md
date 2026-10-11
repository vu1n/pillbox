# OpenCode text delivery gate

The cloud checks validate the adapter, fake-harness refusal/protocol limits,
usage aggregation, credential-free native catalog export, and forced native
tool refusal against a local fake provider with the sealed production profile.
The transport-only test override is verified as loopback before any prompt;
no live model service or credential is involved. These checks do not
establish that a macOS libkrun VM can complete a live provider turn. The draft
must remain gated on that turn and required CI; do not merge or bypass checks.

## Safe preparation on the Mac

Use the draft PR's final reported SHA. Fetching the branch and preparing a
detached worktree needs no credentials or inference:

```sh
OPENCODE_TEXT_SHA='REPLACE_WITH_FINAL_REMOTE_SHA'
git fetch origin codex/text-v2-opencode
git worktree add --detach /tmp/pillbox-opencode-mac "$OPENCODE_TEXT_SHA"
cd /tmp/pillbox-opencode-mac
test "$(git rev-parse HEAD)" = "$OPENCODE_TEXT_SHA"
cargo fmt --check
cargo clippy --all-targets --features libkrun -- -D warnings
cargo test --all-targets --features libkrun
python3 -m unittest discover -s scripts/smoke/opencode-text -p 'test_*.py' -v
cargo build --features libkrun
codesign --force --sign - --entitlements krun/entitlements.plist target/debug/pillbox
scripts/build-runner.sh --tag pillbox-runner:dev
```

The Python suite's two native probes are skipped unless a native CLI path is
provided. They ran in the cloud against OpenCode 2.0.24; a skipped probe on the
Mac must not be reported as passed. To repeat them with an existing matching
Mac CLI, verify its version and run the same suite with the explicit path:

```sh
OPENCODE_NATIVE_BIN=$(command -v opencode)
test "$("$OPENCODE_NATIVE_BIN" --version)" = "2.0.24"
PILLBOX_OPENCODE_TEST_BINARY="$OPENCODE_NATIVE_BIN" \
  python3 -m unittest discover -s scripts/smoke/opencode-text -p 'test_*.py' -v
```

If that CLI is absent or the version differs from the runner pin, report this
probe as blocked rather than prompting a live provider to replace it.

The runner rebuild includes the offline OpenCode catalog export. Its export
starts the bundled CLI with an isolated home, no credentials, registry fetching
disabled, and no prompt; it does not make a provider turn. This is OCI image
preparation for libkrun. It does not add a Docker execution backend.

Use an existing initialized pillbox whose inherited native provider API-key
secret is already authorized for this task. Do not create, reveal, copy, or
log a credential. Supported profiles internally map `openai` to
`OPENAI_API_KEY`/`api.openai.com` and `anthropic` to
`ANTHROPIC_API_KEY`/`api.anthropic.com`; OpenCode SQLite/OAuth credentials are
not accepted. Select an approved model and exact variant present in the
rebuild's catalog. The request generator below never embeds an image, harness
version, catalog, credential reference, or host.

The current profile permits one normally completed model step, native provider
API keys, and text output only. Additional model steps, tool/permission/form
events, empty or incomplete text, and text beyond 32 KiB fail at `turn`; the
adapter never repairs or truncates them. Other provider and OAuth profiles fail
at `resolve`. A native selected model reference does not prove which model the
upstream served, so `served_model` remains null.

## Live turn — parent approval required

The parent coordinates the Mac task and any paid-turn approval. Run the next
command only after that approval, from the selected existing pillbox. Generate
one request with the approved selection first (generation is safe and does not
run inference):

```sh
OPENCODE_LIVE_DIR=$(mktemp -d /tmp/pillbox-opencode-live.XXXXXX)
python3 /tmp/pillbox-opencode-mac/scripts/smoke/opencode-text/request.py \
  --model openai/gpt-6-luna --effort low --output "$OPENCODE_LIVE_DIR/request.json"
```

`openai/gpt-6-luna`/`low` is an example selection; substitute the approved
catalog selection if it differs. After approval, this is the single paid-capable
command, bounded to 120 seconds, 32 KiB final text, 1 MiB native frames and
8 MiB native evidence:

```sh
PILLBOX_RUNNER_IMAGE=pillbox-runner:dev /tmp/pillbox-opencode-mac/target/debug/pillbox \
  text execute --request "$OPENCODE_LIVE_DIR/request.json" > "$OPENCODE_LIVE_DIR/record.json"
python3 /tmp/pillbox-opencode-mac/scripts/smoke/opencode-text/check_record.py \
  "$OPENCODE_LIVE_DIR/record.json"
```

Capture the JSON record, git SHA, image ID, observed harness version, checker
result and referenced session-log/blob digests. Inspect the referenced native
evidence locally to confirm no tool/permission/form events, and the per-step
Usage projection plus turn aggregate. Do not publish secrets or full native
evidence. Verify the owned VM is reaped and no invocation remains running.

An identical retry uses the same request file and returns the original durable
record without another sample; a new generated ID is a new paid-capable turn and
needs its own approval. For a failed record, retain its closed `code`/`stage`
and evidence reference and diagnose before requesting another sample.

## Shared integration footprint

`src/execution/text_v2.rs`: one OpenCode dispatch arm plus generic identity,
input/output and limit validation (including the 32 KiB v2 final-text ceiling).
`src/execution/usage.rs`: `from_opencode_steps`, with per-step deduplication,
running-total/child filtering and visible-output + reasoning aggregation.
`src/execution/mod.rs`, `src/agents/harness/mod.rs` and
`src/sandbox/libkrun/repository.rs`: module declarations only.
`docs/commands.md`: OpenCode support description and one usage-mapping column.
`runner/Dockerfile` and `docs/runner-image.md`: OpenCode-only catalog export.
Claude and other harness implementations are untouched. Apply these small shared
hunks when integrating with the Claude owner; do not replace its driver or usage
implementation. The PR is based on PR180 until that base is merged.
