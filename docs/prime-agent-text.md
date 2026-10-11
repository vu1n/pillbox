# Prime Agent text qualification

`prime-agent` is the Pillbox harness spelling. Its provider is `prime-inference`;
the qualified native release is 0.9.8. Pillbox requests use `pillbox.text/2`,
`tool_policy: "deny_all"`, `placement: "local_microvm"`, and
`output_format: {"type":"text","retry_count":0}`. No Huddles schema changes
are part of this integration.

The runner build observes the bundled native version and obtains the compiled
Prime Inference catalog with offline `get_available_models` RPC, without a
prompt or a real key. It stores the profile in the image. Resolve checks the
exact selected image's profile before looking up managed auth. Model IDs must
match a public Prime Inference catalog entry exactly, optionally prefixed with
`prime-inference/`. Supported request efforts are `off`, `low`, `medium`, `high`;
explicitly unsupported or remapped efforts are rejected instead of clamped.
Version 0.9.8 is an internal qualification rule, never a request field. Older
images without the profile are unavailable; unqualified profiles and unservable
selections are rejected at resolve.

The managed credential reference is global
`auth/prime-agent/.prime/agent/auth.json`. Only its literal `prime-inference`
API-key entry is used. Real keys stay host-side and are released through the
vault solely to `api.pinference.ai`; the guest gets a fresh stub, fresh HOME
and workspace, and no host shares. The native owned-worker frontend avoids
Prime's shared daemon. An empty execution registry includes builtins, custom
tools and MCP; extensions, skills, context files, prompt templates, compaction,
refinement, retries and telemetry are disabled. Native tool attempts fail the
text turn even when the native registry refuses them.

The result requires one normally stopped assistant message, the identical
terminal assistant, successful native exit and complete transport EOF. Empty,
truncated or oversized final text fails at `turn` with
`runtime_protocol_error`; text is never padded or truncated. Byte limits count
UTF-8 bytes, with a hard 32 KiB final-text ceiling. Only native `responseModel`
is authoritative `served_model`; absence yields null. Usage is mapped by
`usage.rs`, once per assistant, with reported cost or null and disjoint token
counts. Failure records contain closed code and stage; diagnostics belong to
session evidence.

## Cloud checks without inference

The native fixture script uses a loopback OpenAI-compatible server, a fixture
provider and nonsecret keys. It asserts that tools are not advertised, forced
bash/edit/Python/MCP calls cannot execute, no sentinel file appears, and success
performs exactly one request with no shared daemon state.

```sh
cargo fmt --check
cargo clippy --no-default-features --all-targets -- -D warnings
cargo test --no-default-features --all-targets
cargo check --all-targets --features libkrun
python3 scripts/smoke/prime-text-native.py \
  --binary /path/to/verified/prime-agent-0.9.8 \
  --output /tmp/pillbox-prime-native-evidence
```

## Required macOS libkrun live gate

Cloud checks do not qualify a live macOS/HVF turn. This gate remains pending
until the Mac owner runs it and returns the result, immutable runner image ID,
native evidence, and confirmed VM teardown. It requires an existing managed
Prime credential and explicit paid-turn approval from the parent. Do not create
credentials or start inference as part of preparing this handoff.

On the isolated branch at the reported remote SHA, first run these preparation
checks. Image assembly is OCI packaging, not the deprecated Docker backend.
Do not use `--update`, which changes other harness pins.

```sh
cargo fmt --check
cargo clippy --all-targets --features libkrun -- -D warnings
cargo test --no-default-features --all-targets
scripts/build-runner.sh --tag pillbox-runner:prime-text-qualification
scripts/lk-build.sh
```

After approval, use a private directory for the request and result. This single
small live request selects the image-owned `openai/gpt-5-nano` with low effort;
if it is unavailable, stop and report resolve failure rather than substituting
a model. The request deliberately has no runtime pins or credential fields.

```sh
umask 077
PRIME_HANDOFF_DIR=$(mktemp -d /tmp/pillbox-prime-live.XXXXXX)
export PRIME_HANDOFF_DIR
python3 - <<'PY'
import hashlib, json, os, pathlib, uuid
directory = pathlib.Path(os.environ['PRIME_HANDOFF_DIR'])
identity = 'prime-live-' + uuid.uuid4().hex
prompt = 'Reply with exactly PILLBOX_PRIME_OK'
request = {
    'contract_version': 'pillbox.text/2',
    'session_ref': {'session_id': identity},
    'invocation_id': identity, 'idempotency_key': identity,
    'rendered_input': prompt,
    'rendered_input_hash': 'sha256:' + hashlib.sha256(prompt.encode()).hexdigest(),
    'tool_policy': 'deny_all',
    'agent': {'harness': 'prime-agent', 'model': 'openai/gpt-5-nano',
              'reasoning_effort': 'low'},
    'placement': 'local_microvm',
    'output_format': {'type': 'text', 'retry_count': 0},
    'limits': {'timeout_ms': 120000, 'max_final_text_bytes': 32768,
               'max_frame_bytes': 1048576, 'max_evidence_bytes': 8388608},
}
(directory / 'request.json').write_text(json.dumps(request) + '\n')
PY
PILLBOX_BACKEND=libkrun PILLBOX_RUNNER_IMAGE=pillbox-runner:prime-text-qualification \
  target/debug/pillbox text execute --request "$PRIME_HANDOFF_DIR/request.json" \
  > "$PRIME_HANDOFF_DIR/result.json"
python3 - <<'PY'
import json, os, pathlib
directory = pathlib.Path(os.environ['PRIME_HANDOFF_DIR'])
record = json.loads((directory / 'result.json').read_text())['execution']
assert record['status'] == 'completed', record
detail = record['detail']
assert detail['output_text'].strip() == 'PILLBOX_PRIME_OK', detail
resolved = detail['resolved']
assert resolved['harness'] == 'prime-agent'
assert resolved['harness_version'] == '0.9.8'
assert resolved['requested_model'] == 'openai/gpt-5-nano'
assert resolved['runner_image_id'].startswith('sha256:')
assert resolved['adapter_revision'] == 'pillbox/prime-agent-text-v1'
assert 'served_model' in resolved
print(json.dumps({'resolved': resolved, 'usage': detail.get('usage'),
                  'session_ref': detail['session_ref']}, indent=2))
PY
# Exact retry verifies durable reuse and must not launch another turn.
PILLBOX_BACKEND=libkrun PILLBOX_RUNNER_IMAGE=pillbox-runner:prime-text-qualification \
  target/debug/pillbox text execute --request "$PRIME_HANDOFF_DIR/request.json" \
  > "$PRIME_HANDOFF_DIR/reused.json"
cmp "$PRIME_HANDOFF_DIR/result.json" "$PRIME_HANDOFF_DIR/reused.json"
```

Return the branch SHA, commands and exit statuses, image ID, sanitized result
and referenced SessionLog/blob evidence to the parent. Any paid adversarial
turn requires separate approval; the executable cloud refusal fixtures already
cover tool execution attempts without billing. No merge, deployment, protection
bypass, or credential material is part of this gate.
