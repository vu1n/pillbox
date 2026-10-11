#!/usr/bin/env python3
"""Offline real-binary qualification against a loopback mock, no provider calls.

Pass an already installed Claude binary. The placeholder API key is accepted
only by this in-process server; no vault, sign-in or subscription is touched.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def run(binary):
    with tempfile.TemporaryDirectory(prefix='pillbox-claude-offline-') as temp:
        marker = Path(temp) / 'tool-executed'
        requests = []
        response_kind = ['text']

        class Mock(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_GET(self):
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b'{}')

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers.get('Content-Length', 0))) or b'{}')
                if self.path.endswith('/count_tokens'):
                    self.send_response(200)
                    self.end_headers()
                    self.wfile.write(b'{"input_tokens":1}')
                    return
                requests.append(body)
                tool = response_kind[0] == 'tool'
                block = ({'type': 'tool_use', 'id': 'toolu_mock', 'name': 'Bash', 'input': {}} if tool
                         else {'type': 'text', 'text': ''})
                delta = ({'type': 'input_json_delta', 'partial_json': json.dumps({'command': 'touch ' + str(marker)})} if tool
                         else {'type': 'text_delta', 'text': 'PILLBOX_OFFLINE_OK'})
                message = {'id': 'msg_mock', 'type': 'message', 'role': 'assistant',
                           'model': 'claude-opus-4-8', 'content': [], 'stop_reason': None,
                           'stop_sequence': None, 'usage': {'input_tokens': 1, 'output_tokens': 1}}
                frames = [
                    ('message_start', {'type': 'message_start', 'message': message}),
                    ('content_block_start', {'type': 'content_block_start', 'index': 0, 'content_block': block}),
                    ('content_block_delta', {'type': 'content_block_delta', 'index': 0, 'delta': delta}),
                    ('content_block_stop', {'type': 'content_block_stop', 'index': 0}),
                    ('message_delta', {'type': 'message_delta', 'delta': {'stop_reason': 'tool_use' if tool else 'end_turn', 'stop_sequence': None}, 'usage': {'output_tokens': 1}}),
                    ('message_stop', {'type': 'message_stop'}),
                ]
                encoded = ''.join(f'event: {name}\ndata: {json.dumps(data)}\n\n' for name, data in frames).encode()
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Content-Length', str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)

        server = ThreadingHTTPServer(('127.0.0.1', 0), Mock)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            argv = [str(binary), '-p', '--output-format', 'stream-json', '--verbose',
                    '--tools', '', '--strict-mcp-config', '--mcp-config', '{"mcpServers":{}}',
                    '--disable-slash-commands', '--safe-mode', '--setting-sources', '',
                    '--settings', '{"disableAllHooks":true,"permissions":{"deny":["*"]}}',
                    '--permission-mode', 'default', '--permission-prompts', 'none',
                    '--no-session-persistence', '--max-turns', '1',
                    '--model', 'claude-opus-4-8', '--effort', 'low']
            env = {'PATH': os.environ['PATH'], 'CLAUDE_CONFIG_DIR': temp,
                   'ANTHROPIC_API_KEY': 'offline-test-placeholder',
                   'ANTHROPIC_BASE_URL': f'http://127.0.0.1:{server.server_port}',
                   'DISABLE_AUTOUPDATER': '1', 'DISABLE_TELEMETRY': '1',
                   'DISABLE_ERROR_REPORTING': '1', 'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1'}
            for kind in ('text', 'tool'):
                response_kind[0] = kind
                result = subprocess.run(argv, input='Reply briefly without tools.', text=True,
                                        capture_output=True, cwd=temp, env=env, timeout=30)
                lines = [json.loads(line) for line in result.stdout.splitlines()]
                init = next(line for line in lines if line.get('subtype') == 'init')
                assert init['tools'] == [] and init['mcp_servers'] == [], init
                assert not marker.exists(), 'Claude executed a disabled tool'
                assert requests and all(not request.get('tools') for request in requests)
                if kind == 'text':
                    terminal = next(line for line in lines if line.get('type') == 'result')
                    assert result.returncode == 0 and terminal['is_error'] is False, terminal
                    assert terminal['permission_denials'] == [], terminal
                    assert terminal['num_turns'] == 1, terminal
                    assert terminal['result'] == 'PILLBOX_OFFLINE_OK', terminal
                    assert not any(block.get('type') in ('tool_use', 'tool_result')
                                   for line in lines for block in line.get('message', {}).get('content', []))
                print(f'{kind}: empty advertised tools/MCP; no tool executed; version={init["claude_code_version"]}')
        finally:
            server.shutdown()
            server.server_close()
            worker.join()


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--claude', required=True, type=Path)
    run(parser.parse_args().claude.resolve())
