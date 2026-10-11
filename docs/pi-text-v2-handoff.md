# Pi text/2 macOS libkrun gate

The cloud implementation is **not done** until this gate passes on macOS with
libkrun/HVF. Linux fake transport/provider events do not prove guest startup,
Node custom-CA trust, vault substitution, or authenticated provider acceptance.
The parent coordinates the Mac task and paid-turn approval. Do not merge,
create/display credentials, bypass protection, or use the Docker agent backend.

## Prepare without inference

Fetch `codex/pi-text-v2` into an isolated worktree. Confirm `git rev-parse HEAD`
matches the implementation thread's final remote SHA. Read `CLAUDE.md`,
`AGENTS.md`, and this handoff.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
node --test tests/pi_text_driver.test.mjs
python3 -m unittest discover -s tests -p 'pi_text_bridge_test.py'
PI_BUNDLE_DIR=$(mktemp -d "${TMPDIR:-/tmp}/pi-bundle.XXXXXX")
npm pack @earendil-works/pi-coding-agent@1.0.2 --ignore-scripts --pack-destination "$PI_BUNDLE_DIR" --silent
tar -xzf "$PI_BUNDLE_DIR/earendil-works-pi-coding-agent-1.0.2.tgz" -C "$PI_BUNDLE_DIR"
node tests/pi_text_bundle.mjs "$PI_BUNDLE_DIR/package"
cargo build
codesign --entitlements krun/entitlements.plist -f -s - target/debug/pillbox
```

The configured runner image must already contain the qualified Pi 1.0.2 SDK.
Use the existing `[runner].image` or `PILLBOX_RUNNER_IMAGE`; neither belongs in
the request. Docker supplies OCI identity/materialization only; all execution
uses libkrun. Do not rebuild/publish an image or create auth to make this pass.

Generate two requests locally without inference:

```sh
PI_TEXT_HANDOFF_DIR=$(mktemp -d "${TMPDIR:-/tmp}/pi-text-v2.XXXXXX")
python3 - "$PI_TEXT_HANDOFF_DIR" <<'PY'
import hashlib, json, pathlib, sys, uuid
root = pathlib.Path(sys.argv[1])
for kind, model, text in [
    ('unservable', 'pillbox-pi-model-does-not-exist', 'No inference must run.'),
    ('live', 'gpt-6-luna', 'Return exactly PILLBOX_PI_TEXT_V2_OK. Do not call tools.'),
]:
    identity = 'pi-text-' + kind + '-' + uuid.uuid4().hex
    request = {
        'contract_version': 'pillbox.text/2', 'session_ref': {'session_id': identity},
        'invocation_id': identity, 'idempotency_key': identity,
        'rendered_input': text,
        'rendered_input_hash': 'sha256:' + hashlib.sha256(text.encode()).hexdigest(),
        'tool_policy': 'deny_all',
        'agent': {'harness': 'pi', 'model': model, 'reasoning_effort': 'low'},
        'placement': 'local_microvm', 'output_format': {'kind': 'text', 'retry_count': 0},
        'limits': {'timeout_ms': 120000, 'max_final_text_bytes': 32768,
                   'max_frame_bytes': 1048576, 'max_evidence_bytes': 8388608},
    }
    (root / (kind + '.json')).write_text(json.dumps(request))
PY
target/debug/pillbox text execute --request "$PI_TEXT_HANDOFF_DIR/unservable.json" > "$PI_TEXT_HANDOFF_DIR/unservable-result.json"
```

Verify `status=failed`, `stage=resolve`, `code=runtime_rejected`, an evidence
reference, and no free-text failure field. This probe reads no credentials and
makes no provider request. Use fresh invocation/session IDs for independent
tests; reuse the exact original only for recovery/idempotency checks.

Prove missing credentials in a separate test OS account/HOME with no Pillbox
auth and its own state. Provision no credentials; do not remove/rename the
operator's auth state. Execute the valid live request there: it must fail
`runtime_unavailable` at `credentials` after offline model resolution, without
a provider call. The synthetic Linux credential test does not replace this test.

## One approved live turn

**Only after the parent records paid-turn authorization**, execute once with
existing fresh managed Codex credentials through the vault. Renewal-due credentials
fail at `credentials`; use the existing host broker to renew outside the bounded
turn, without displaying or creating credentials:

```sh
target/debug/pillbox text execute --request "$PI_TEXT_HANDOFF_DIR/live.json" > "$PI_TEXT_HANDOFF_DIR/live-result.json"
```

Verify `status=completed`, `output_text=PILLBOX_PI_TEXT_V2_OK`, text at most
32 KiB, and all six resolved fields. `harness=pi`, observed version `1.0.2`,
adapter `pillbox/pi-text-v1`, and the actual immutable image ID must agree with
the configured image. Requested model matches the request; served model is
provider-reported or null. Compare usage and the `turn_usage` event against
captured native usage and Pi terminal cost: no doubled cache subtraction, synthetic prices,
or initialized zero usage.

Inspect restricted evidence locally: no tool dispatch, retries, compaction,
cache warming, or second provider call; no shares; provider-only egress; real
bearer/account/refresh values absent from guest artifacts. Do not paste tokens
or auth files into a report. Confirm both owned VMMs stopped and private roots
were removed. Report exact SHA, invocation ID, command exits, sanitized
resolved/usage metadata, elapsed time, and evidence reference. On failure,
preserve evidence, report closed code/stage, and stop. Obtain parent approval
before a new paid attempt.

Hosted jobs blocked by exhausted Actions minutes are **unstarted**, not failing
tests. Report whether jobs/steps executed and preserve required protections.
