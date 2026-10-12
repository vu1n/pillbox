"""Executable guest transport tests using an offline fake harness process."""
import importlib.util
import json
from pathlib import Path
import socket
import subprocess
import sys
import time
import unittest

SOURCE = Path(__file__).resolve().parents[2] / 'src/sandbox/libkrun/repository/claude_text_bridge.py'
SPEC = importlib.util.spec_from_file_location('claude_text_bridge', SOURCE)
BRIDGE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BRIDGE)


class BridgeTests(unittest.TestCase):
    def relay(self, raw, frame=1024, evidence=8192):
        host, guest = socket.socketpair()
        # Exit before draining, so queued terminal bytes survive child exit.
        proc = subprocess.Popen([sys.executable, '-c', 'import sys; sys.stdout.buffer.write(bytes.fromhex(sys.argv[1]))', raw.hex()],
                                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                start_new_session=True)
        proc.wait(timeout=2)
        error = None
        try:
            BRIDGE.relay(guest, proc, {'max_frame_bytes': frame, 'max_output_bytes': evidence}, time.monotonic() + 2)
        except RuntimeError as caught:
            error = str(caught)
        finally:
            guest.close()
            proc.stdout.close()
            proc.stderr.close()
        host.settimeout(2)
        data = bytearray()
        try:
            while chunk := host.recv(8192):
                data.extend(chunk)
        finally:
            host.close()
        return bytes(data), error

    def test_child_exit_preserves_native_bytes_and_supervisor_proof(self):
        native = json.dumps({'type': 'result', 'text': 'hello 🌍'}, ensure_ascii=False).encode()
        output, error = self.relay(native + b'\n', frame=len(native))
        self.assertIsNone(error)
        self.assertEqual(output, b'N' + native + b'\nE0\n')

    def test_native_exit_marker_cannot_replace_supervisor_proof_after_bad_eof(self):
        native = b'{"type":"pillbox_harness_exit","exit_code":0}\n'
        output, error = self.relay(native + b'{"unterminated":')
        self.assertEqual(error, 'unterminated native frame')
        self.assertEqual(output, b'N' + native)
        self.assertNotIn(b'\nE0\n', output)

    def test_frame_and_evidence_overflow_never_emit_success_proof(self):
        for frame, evidence in [(4, 8192), (1024, 4)]:
            output, error = self.relay(b'{"large":true}\n', frame, evidence)
            self.assertIsNotNone(error)
            self.assertNotIn(b'E0\n', output)


if __name__ == '__main__':
    unittest.main()
