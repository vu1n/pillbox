"""Offline synthetic evidence tests. Passing these is not a runtime/provider gate."""
import base64
import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from fingerprint_state import fingerprint
from inspect_completion import inspect_completion
from prepare import FILES, canonical, canonical_digest, digest, manifest, prepare
import smoke

IMAGE = 'sha256:' + '3a' * 32
PATCH = b'''diff --git a/src/answer.txt b/src/answer.txt
--- a/src/answer.txt
+++ b/src/answer.txt
@@ -1 +1 @@
-answer=0
+answer=42
'''


def fake_completion(root, state, encoding='utf8', change_frames=None, patch_bytes=PATCH):
    """Construct hash-correct local fixtures so the checker, not an RPC stub, runs."""
    request = json.loads((root / 'generated/request.json').read_text())
    identities = json.loads((root / 'generated/identities.json').read_text())
    builder, verifier = request['session_ref']['session_id'], 'fixture-verifier-session'
    for session in (builder, verifier):
        path = state / 'sessions' / session
        (path / 'blobs').mkdir(parents=True)
        (path / 'log.jsonl').write_text(json.dumps({'seq': 1}) + '\n')

    def ref(session):
        return {'session_id': session, 'seq_range': [1, 1]}

    def artifact(data, session=builder):
        checksum = digest(data)
        (state / 'sessions' / session / 'blobs' / checksum[7:]).write_bytes(data)
        return {'session_id': session, 'digest': checksum, 'bytes': len(data), 'media_type': 'fixture'}

    expected = {**FILES, 'src/answer.txt': b'answer=42\n'}
    for content in expected.values():
        artifact(content)
    admission = {key: identities[key] for key in ('request_hash', 'manifest_digest', 'input_snapshot_digest')}
    admission.update(invocation_id=request['invocation_id'], runner_image_id=IMAGE, evidence=ref(builder))
    result = {key: admission[key] for key in ('invocation_id', 'request_hash', 'manifest_digest')}
    result.update(base=request['manifest']['base'], execution=request['execution'],
                  output_id=request['manifest']['output_id'], changed_paths=['src/answer.txt'],
                  result_snapshot_digest=identities['expected_result_snapshot_digest'],
                  snapshot_manifest=artifact(canonical(manifest(expected)).encode()), patch=artifact(patch_bytes),
                  evidence=ref(builder))
    verification = {key: request['manifest']['verifier'][key] for key in ('verifier_id', 'run_id', 'definition_digest')}
    verification.update(output_id=result['output_id'], result_digest=canonical_digest(result),
                        result_snapshot_digest=result['result_snapshot_digest'], outcome='pass', evidence=ref(verifier))
    report = {key: verification[key] for key in ('verifier_id', 'run_id', 'definition_digest', 'output_id', 'result_digest', 'result_snapshot_digest')}
    report.update(version='pillbox.verifier/1', exit_code=0, signal=None, timed_out=False, output_limited=False,
                  stdout_base64=base64.b64encode(b'checked').decode(), stderr_base64='')
    verification['report'] = artifact((canonical(report) + '\n').encode(), verifier)
    frames = [
        {'direction': 'inbound', 'message': {'id': 'pillbox-thread', 'result': {
            'model': 'gpt-5.6-sol', 'reasoningEffort': 'low', 'modelProvider': 'pillbox_openai_http',
            'thread': {'id': 'thread-1', 'cliVersion': '0.151.0'}}}},
        {'direction': 'outbound', 'message': {'method': 'turn/start', 'params': {
            'model': 'gpt-5.6-sol', 'effort': 'low', 'input': [{'text': request['rendered_input']}]}}},
    ]
    for index, path in enumerate(('docs/task.txt', 'src/answer.txt', 'src/answer.txt')):
        args = {'path': path}
        if index == 2:
            args.update(executable=False, encoding=encoding, content='answer=42\n' if encoding == 'utf8' else base64.b64encode(b'answer=42\n').decode())
        frames.append({'direction': 'inbound', 'message': {'method': 'item/tool/call', 'params': {
            'threadId': 'thread-1', 'turnId': 'turn-1', 'callId': str(index),
            'tool': 'pillbox_write_file' if index == 2 else 'pillbox_read_file', 'arguments': args}}})
    frames.append({'direction': 'inbound', 'message': {'method': 'turn/completed', 'params': {
        'threadId': 'thread-1', 'turn': {'id': 'turn-1', 'status': 'completed', 'error': None}}}})
    if change_frames:
        change_frames(frames)
    native = artifact(b''.join((canonical(frame) + '\n').encode() for frame in frames))
    completion = {'execution': {'invocation_id': request['invocation_id'], 'request_hash': identities['request_hash'],
                               'status': 'completed', 'detail': {'admission': admission, 'result': result,
                               'verification': verification, 'native_evidence': native, 'text': artifact(b'done')}}}
    path = root / 'completion.json'
    path.write_text(json.dumps(completion))
    return path, completion


class SmokeTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='repository-smoke-test-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'artifacts with spaces'
        self.state = Path(self.temp.name) / 'state'
        prepare(self.root, IMAGE)

    def test_prepare_is_portable_deterministic_and_refuses_changed_inputs(self):
        other = Path(self.temp.name) / 'other'
        prepare(other, IMAGE)
        self.assertEqual((self.root / 'generated/request.json').read_bytes(), (other / 'generated/request.json').read_bytes())
        prepare(self.root, IMAGE)
        with self.assertRaises(AssertionError):
            prepare(self.root, 'image:latest')
        (self.root / 'repository/src/answer.txt').write_text('dirty\n')
        with self.assertRaises(AssertionError):
            prepare(self.root, IMAGE)

    def test_new_attempt_requires_explicit_identity_and_fresh_artifacts(self):
        other = Path(self.temp.name) / 'new attempt'
        prepare(other, IMAGE, 'v2')
        request = json.loads((other / 'generated/request.json').read_text())
        self.assertEqual(request['invocation_id'], 'smoke-execution-v2')
        self.assertEqual(request['idempotency_key'], request['invocation_id'])
        self.assertEqual(request['session_ref']['session_id'], 'smoke-builder-session-v2')
        prepare(other, IMAGE, 'v2')
        with self.assertRaises(AssertionError):
            prepare(self.root, IMAGE, 'v2')
        with self.assertRaises(AssertionError):
            prepare(other, IMAGE, '../invalid')

    def test_shell_defaults_to_offline_preparation(self):
        script = Path(__file__).resolve().parent.parent / 'repository-execution.sh'
        output = subprocess.run(['bash', str(script), '--artifacts', str(self.root), '--image', IMAGE],
                                check=True, capture_output=True, timeout=10)
        self.assertEqual(json.loads(output.stdout)['snapshot_digest'], json.loads((self.root / 'generated/identities.json').read_text())['input_snapshot_digest'])
        self.assertFalse((self.root / 'observations').exists())

    def test_inspector_checks_actual_utf8_and_base64_artifacts_and_patch(self):
        for encoding in ('utf8', 'base64'):
            state = self.state / encoding
            path, _ = fake_completion(self.root, state, encoding)
            result = inspect_completion(self.root, path, state, path)
            self.assertTrue(result['patch_reproduces_complete_snapshot'])
            self.assertEqual(result['native_turn_start_count'], 1)
            self.assertTrue(result['retry_record_equal'])

    def test_inspector_rejects_hash_correct_wrong_patch(self):
        wrong_patches = [PATCH.replace(b'+answer=42', b'+answer=99'),
                         PATCH.replace(b'--- a/', b'old mode 100644\nnew mode 100755\n--- a/', 1)]
        for index, changed in enumerate(wrong_patches):
            state = self.state / str(index)
            path, _ = fake_completion(self.root, state, patch_bytes=changed)
            with self.assertRaisesRegex(AssertionError, 'patch does not reproduce'):
                inspect_completion(self.root, path, state)

    def test_inspector_rejects_blob_tampering_and_shared_verifier_session(self):
        path, record = fake_completion(self.root, self.state)
        artifact = record['execution']['detail']['text']
        blob = self.state / 'sessions' / artifact['session_id'] / 'blobs' / artifact['digest'][7:]
        blob.write_bytes(b'forged')
        with self.assertRaises(AssertionError):
            inspect_completion(self.root, path, self.state)
        blob.write_bytes(b'done')
        detail = record['execution']['detail']
        detail['verification']['evidence'] = detail['result']['evidence']
        path.write_text(json.dumps(record))
        with self.assertRaises(AssertionError):
            inspect_completion(self.root, path, self.state)

    def test_inspector_rejects_duplicate_turn_model_change_and_outside_grant(self):
        mutations = [lambda f: f.append(copy.deepcopy(f[1])),
                     lambda f: f[0]['message']['result'].update(model='another-model'),
                     lambda f: f[2]['message']['params']['arguments'].update(path='private/ungranted.txt')]
        for index, mutate in enumerate(mutations):
            state = self.state / str(index)
            path, _ = fake_completion(self.root, state, change_frames=mutate)
            with self.assertRaises(AssertionError):
                inspect_completion(self.root, path, state)

    def test_fingerprint_ignores_auth_and_detects_evidence_changes(self):
        path, _ = fake_completion(self.root, self.state)
        before = fingerprint(self.state)
        (self.state / 'auth').mkdir()
        (self.state / 'auth/never-read').write_bytes(b'synthetic')
        self.assertEqual(fingerprint(self.state), before)
        log = next((self.state / 'sessions').glob('*/log.jsonl'))
        log.write_text(log.read_text() + '{}\n')
        self.assertNotEqual(fingerprint(self.state), before)

    def test_failed_command_outputs_survive_and_live_never_relaunches(self):
        directory = self.root / 'observations/live'
        directory.mkdir(parents=True)
        with self.assertRaises(AssertionError):
            smoke.capture(directory, 'failure', [sys.executable, '-c', 'print("kept"); raise SystemExit(17)'], self.root)
        self.assertEqual((directory / 'failure.json').read_text(), 'kept\n')
        self.assertEqual(json.loads((directory / 'failure.command.json').read_text())['exit_code'], 17)
        with patch.object(smoke, 'runtime_identity') as runtime:
            with self.assertRaises(FileExistsError):
                smoke.run_live(type('Args', (), {'artifacts': self.root, 'image': IMAGE, 'binary': Path('/unused'), 'attempt': 'v1'})())
            runtime.assert_not_called()


if __name__ == '__main__':
    unittest.main()
