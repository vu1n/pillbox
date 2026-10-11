"""Offline tests of the exact guest relay; fake processes make no model calls."""
import importlib.util
import json
import pathlib
import socket
import subprocess
import sys
import threading
import time
import unittest

PATH = pathlib.Path(__file__).resolve().parents[1] / 'src/execution/pi_text/bridge.py'
SPEC = importlib.util.spec_from_file_location('pi_bridge', PATH)
BRIDGE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BRIDGE)


class RelayTests(unittest.TestCase):
    def run_relay(self, code, request=None, frame=4096, evidence=8192, timeout=2):
        host, guest = socket.socketpair()
        host.settimeout(3)
        errors = []
        proc = subprocess.Popen([sys.executable, '-I', '-S', '-c', code],
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, bufsize=0, start_new_session=True)

        def worker():
            try:
                BRIDGE.relay(guest, proc, {'max_frame_bytes': frame, 'max_output_bytes': evidence},
                             time.monotonic() + timeout, request)
            except Exception as error:
                errors.append(error)
            finally:
                guest.close()
                BRIDGE.stop(proc)

        thread = threading.Thread(target=worker)
        thread.start()
        captured = bytearray()
        try:
            while True:
                data = host.recv(65536)
                if not data:
                    break
                captured.extend(data)
        finally:
            host.close()
            thread.join(timeout=4)
            self.assertFalse(thread.is_alive(), 'owned relay failed to stop')
            for pipe in (proc.stdin, proc.stdout, proc.stderr):
                if not pipe.closed:
                    pipe.close()
        return bytes(captured), errors

    def test_exact_utf8_request_final_stream_and_exit(self):
        raw, errors = self.run_relay(
            'import json,sys; request=json.load(sys.stdin); '
            'print(json.dumps({"type":"answer","text":request["input"]})); '
            'print(json.dumps({"type":"pillbox_pi.done"}))', {'input': 'é' * 100})
        self.assertEqual(errors, [])
        events = [json.loads(line) for line in raw.splitlines()]
        self.assertEqual(events[0]['text'], 'é' * 100)
        self.assertEqual(events[-1], {'type': 'pillbox_pi.exit', 'code': 0})

    def test_frame_and_evidence_limits_fail_without_truncating_into_success(self):
        for frame, evidence, output in [
            (10, 8192, 'x' * 11 + '\n'),
            (4096, 10, '{"type":"event"}\n'),
        ]:
            raw, errors = self.run_relay('import sys; sys.stdout.write(' + repr(output) + ')',
                                         frame=frame, evidence=evidence)
            self.assertTrue(errors)
            self.assertNotIn(b'pillbox_pi.exit', raw)

    def test_unterminated_frame_and_nonzero_process_exit(self):
        raw, errors = self.run_relay('print("{}",end="")')
        self.assertTrue(errors)
        self.assertNotIn(b'pillbox_pi.exit', raw)
        raw, errors = self.run_relay('raise SystemExit(7)')
        self.assertEqual(errors, [])
        self.assertEqual(json.loads(raw), {'type': 'pillbox_pi.exit', 'code': 7})

    def test_deadline_stops_an_owned_process(self):
        raw, errors = self.run_relay('import time; time.sleep(10)', timeout=0.05)
        self.assertTrue(errors)
        self.assertEqual(raw, b'')
        self.assertIn('deadline exceeded', str(errors[0]))


if __name__ == '__main__':
    unittest.main()
