#!/usr/bin/env python3
"""Exercise the actual Prime native binary against a loopback-only provider.

No real credentials, auth login, provider traffic, or paid inference. The
fixture forces disallowed tools despite an empty advertised tool registry,
checks filesystem effects, and verifies the emitted final/usage contract.
"""

import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


SETTINGS = {
    "compaction": {"enabled": False, "agentCallable": False},
    "autoRefine": {"enabled": False, "compact": False},
    "retry": {"enabled": False, "provider": {"waitForUsage": {"enabled": False}}},
    "agentTraces": {"enabled": False},
    "telemetry": {"enabled": False, "noticeShown": True},
}


def probe(binary, denied, output):
    requests = []
    with tempfile.TemporaryDirectory(prefix="pillbox-prime-native-") as directory:
        root = Path(directory)
        canary = root / "must-not-exist"
        agent = root / "agent"
        agent.mkdir()
        (agent / "settings.json").write_text(json.dumps(SETTINGS))

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                requests.append(payload)
                calls = [
                    ("bash", {"command": f"touch {canary}"}),
                    ("edit", {"path": str(canary), "oldText": "", "newText": "unsafe"}),
                    ("ipython", {"code": f"open({str(canary)!r}, 'w').write('unsafe')"}),
                    ("mcp__canary", {"url": "https://must-not-connect.invalid"}),
                ]
                if denied and len(requests) == 1:
                    delta = {"role": "assistant", "tool_calls": [
                        {"index": index, "id": f"call_{name}", "type": "function",
                         "function": {"name": name, "arguments": json.dumps(arguments)}}
                        for index, (name, arguments) in enumerate(calls)
                    ]}
                    reason = "tool_calls"
                else:
                    delta = {"role": "assistant", "content": "Safe final answer."}
                    reason = "stop"
                chunks = [
                    {"index": 0, "delta": delta, "finish_reason": None},
                    {"index": 0, "delta": {}, "finish_reason": reason},
                ]
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                for choice in chunks:
                    chunk = {"id": "mock", "object": "chat.completion.chunk", "created": 1,
                             "model": "observed-mock-model", "choices": [choice]}
                    self.wfile.write(("data: " + json.dumps(chunk) + "\n\n").encode())
                usage = {"id": "mock", "object": "chat.completion.chunk", "created": 1,
                         "model": "observed-mock-model", "choices": [], "usage": {
                             "prompt_tokens": 100, "completion_tokens": 5,
                             "prompt_tokens_details": {"cached_tokens": 20, "cache_write_tokens": 10}}}
                self.wfile.write(("data: " + json.dumps(usage) + "\n\ndata: [DONE]\n\n").encode())

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        models = {"providers": {"pillbox-fixture": {
            "baseUrl": f"http://127.0.0.1:{server.server_port}/v1",
            "api": "openai-completions", "apiKey": "fixture-no-credential",
            "models": [{"id": "mock-model", "name": "mock-model", "reasoning": False,
                        "input": ["text"], "contextWindow": 4096, "maxTokens": 256,
                        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                        "compat": {"supportsUsageInStreaming": True,
                                   "supportsDeveloperRole": False, "supportsStore": False}}]}}}
        (agent / "models.json").write_text(json.dumps(models))
        environment = {
            "PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": directory,
            "PRIME_AGENT_CODING_AGENT_DIR": str(agent), "PI_OFFLINE": "1", "DO_NOT_TRACK": "1",
            "PRIME_AGENT_INTERNAL_LEGACY_OWNED_WORKER_FRONTEND": "1",
        }
        argv = [str(binary), "-p", "--mode", "json", "--provider", "pillbox-fixture",
                "--model", "mock-model", "--no-session", "--no-tools", "--no-extensions",
                "--no-skills", "--no-context-files", "--no-prompt-templates", "--offline",
                "--", "Complete this fixture request as text."]
        process = subprocess.Popen(argv, env=environment, cwd=directory, text=True,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   stdin=subprocess.DEVNULL, start_new_session=True)
        try:
            stdout, stderr = process.communicate(timeout=30)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.communicate()
            raise
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
        if output:
            output.mkdir(parents=True, exist_ok=True)
            name = "denied" if denied else "success"
            (output / f"{name}.jsonl").write_text(stdout)
        assert process.returncode == 0, stderr
        assert not stderr, stderr
        assert len(requests) == (2 if denied else 1), "unexpected background/model call"
        assert all(not request.get("tools") for request in requests), "tools advertised"
        assert not canary.exists(), "disallowed tool executed"
        assert not (root / ".prime/supervisor-owners").exists(), "shared daemon was started"
        events = [json.loads(line) for line in stdout.splitlines()]
        terminal = [event for event in events if event.get("type") == "agent_end"]
        assert len(terminal) == 1, "compaction/refinement/continuation added a run"
        messages = terminal[0]["messages"]
        results = [message for message in messages if message.get("role") == "toolResult"]
        assert len(results) == (4 if denied else 0)
        assert all(message["isError"] and "not found" in "".join(
            block.get("text", "") for block in message["content"]) for message in results)
        final = next(message for message in reversed(messages) if message.get("role") == "assistant")
        assert final["stopReason"] == "stop"
        assert final["responseModel"] == "observed-mock-model"
        assert final["content"] == [{"type": "text", "text": "Safe final answer."}]
        assert {field: final["usage"][field] for field in ["input", "output", "cacheRead", "cacheWrite"]} == {
            "input": 80, "output": 5, "cacheRead": 10, "cacheWrite": 10}
        print(f"PASS {'denied bash/edit/ipython/MCP' if denied else 'success'}: native final, usage, isolated worker")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True, help="Verified native Prime Agent executable")
    parser.add_argument("--output", type=Path, help="Optional no-secret native JSONL evidence directory")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    for denied in [False, True]:
        probe(binary, denied, args.output)


if __name__ == "__main__":
    main()
