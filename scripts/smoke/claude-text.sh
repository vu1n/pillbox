#!/usr/bin/env bash
# Requires explicit live-turn authorization from the coordinating task.
set -euo pipefail
if [[ "${1:-}" != "--live" || "$(uname -s)" != Darwin ]]; then
    echo "Usage on macOS after live-turn approval: $0 --live /path/to/pillbox [evidence-dir]" >&2
    exit 2
fi
pillbox_bin="${2:?provide the freshly built, codesigned libkrun binary}"
evidence_dir="${3:-$(mktemp -d "${TMPDIR:-/tmp}/pillbox-claude-live.XXXXXX")}"
mkdir -p "$evidence_dir"
chmod 700 "$evidence_dir"
python3 - "$evidence_dir/request.json" <<'PY'
import hashlib, json, sys, uuid
invocation = 'claude-text-' + uuid.uuid4().hex
prompt = 'Reply with exactly PILLBOX_CLAUDE_TEXT_OK. Do not use any tool.'
request = {
    'contract_version': 'pillbox.text/2',
    'session_ref': {'session_id': invocation},
    'invocation_id': invocation, 'idempotency_key': invocation,
    'rendered_input': prompt,
    'rendered_input_hash': 'sha256:' + hashlib.sha256(prompt.encode()).hexdigest(),
    'tool_policy': 'deny_all',
    'agent': {'harness': 'claude_code', 'model': 'claude-opus-4-8', 'reasoning_effort': 'low'},
    'placement': 'local_microvm', 'output_format': {'kind': 'text', 'retry_count': 0},
    'limits': {'timeout_ms': 120000, 'max_final_text_bytes': 32768,
               'max_frame_bytes': 1048576, 'max_evidence_bytes': 8388608},
}
with open(sys.argv[1], 'w') as handle:
    json.dump(request, handle)
PY
chmod 600 "$evidence_dir/request.json"
PILLBOX_BACKEND=libkrun "$pillbox_bin" text execute --request "$evidence_dir/request.json" > "$evidence_dir/result.json"
session_id="$(python3 - "$evidence_dir/result.json" <<'PY'
import json, sys
record = json.load(open(sys.argv[1]))['execution']
assert record['status'] == 'completed', record
detail = record['detail']
assert detail['output_text'].strip() == 'PILLBOX_CLAUDE_TEXT_OK', detail
assert len(detail['output_text'].encode()) <= 32768
resolved = detail['resolved']
assert set(resolved) == {'harness', 'harness_version', 'adapter_revision', 'runner_image_id', 'requested_model', 'served_model'}
assert resolved['harness'] == 'claude_code' and resolved['harness_version']
assert resolved['runner_image_id'].startswith('sha256:')
assert resolved['requested_model'] == 'claude-opus-4-8'
assert resolved['served_model'] is None or isinstance(resolved['served_model'], str)
if 'usage' in detail:
    assert 'cost_usd' in detail['usage']
print(detail['session_ref']['session_id'])
PY
)"
# Foreground text sessions have a durable log, without a detached registry row.
# Read exactly the reported session/blob references through the selected state dir.
"$pillbox_bin" info --json > "$evidence_dir/pillbox-info.json"
python3 - "$evidence_dir" "$session_id" <<'PY'
import hashlib, json, re, shutil, sys
from pathlib import Path
destination = Path(sys.argv[1])
state = Path(json.load(open(destination / 'pillbox-info.json'))['pillbox']['state_dir'])
session = state / 'sessions' / sys.argv[2]
shutil.copyfile(session / 'log.jsonl', destination / 'session.jsonl')
native_ref = None
for line in open(destination / 'session.jsonl'):
    event = json.loads(line)
    payload = event['payload']
    assert payload['type'] != 'tool_call', event
    if payload['type'] == 'custom' and payload.get('name') == 'text.result.captured':
        native_ref = payload['payload']['native_evidence']
assert native_ref is not None, 'missing native evidence reference'
digest = native_ref['digest'].removeprefix('sha256:')
assert re.fullmatch('[0-9a-f]{64}', digest)
raw = (session / 'blobs' / digest).read_bytes()
assert hashlib.sha256(raw).hexdigest() == digest and len(raw) == native_ref['bytes']
(destination / 'native.jsonl').write_bytes(raw)
messages = [json.loads(line)['message'] for line in raw.splitlines()]
result = [message for message in messages if message.get('type') == 'result']
assert len(result) == 1 and result[0]['permission_denials'] == []
assert result[0]['stop_reason'] == 'end_turn' and result[0]['terminal_reason'] == 'completed'
for message in messages:
    assert message.get('subtype') != 'permission_denied'
    assert not any(block.get('type') in ('tool_use', 'tool_result')
                   for block in message.get('message', {}).get('content', []))
print('Live Claude text gate passed: completed bounded text, resolved metadata, no tool events.')
PY
echo "Evidence: $evidence_dir (retain private; native evidence contains prompt/output)."
