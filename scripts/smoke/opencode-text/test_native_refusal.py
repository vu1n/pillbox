"""Native refusal against a loopback fake provider; never contacts a model service."""
import http.server
import json
import os
from pathlib import Path
import re
import threading
import time
import unittest
from unittest.mock import patch

from test_bridge import BRIDGE, ROOT


def response_events(name, arguments):
    item = {"id": "fc_fixture", "type": "function_call", "call_id": "call_fixture",
            "name": name, "arguments": json.dumps(arguments), "status": "completed"}
    return [
        {"type": "response.created", "response": {"id": "resp_fixture"}},
        {"type": "response.output_item.added", "output_index": 0, "item": {**item, "arguments": ""}},
        {"type": "response.function_call_arguments.delta", "item_id": item["id"],
         "output_index": 0, "delta": item["arguments"]},
        {"type": "response.output_item.done", "output_index": 0, "item": item},
        {"type": "response.completed", "response": {"id": "resp_fixture", "status": "completed",
         "output": [item], "usage": {"input_tokens": 1, "output_tokens": 1,
         "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}}}},
    ]


@unittest.skipUnless(os.environ.get("PILLBOX_OPENCODE_TEST_BINARY"), "requires the runner's native CLI")
class NativeRefusalTests(unittest.TestCase):
    def test_sealed_profile_offers_no_tools_and_refuses_forced_calls(self):
        import tempfile
        with tempfile.TemporaryDirectory() as root:
            sentinel = Path(root) / "must-not-exist"
            original = Path(root) / "must-not-change"
            original.write_text("unchanged")
            cases = {
                "bash": {"command": "touch " + str(sentinel), "description": "refusal fixture"},
                "shell": {"command": "touch " + str(sentinel), "description": "refusal fixture"},
                "read": {"path": str(original)},
                "write": {"path": str(sentinel), "content": "forbidden"},
                "edit": {"path": str(original), "oldString": "unchanged", "newString": "forbidden"},
                "webfetch": {"url": "", "format": "text"},
                "mcp_private_tool": {},
                "execute": {"code": "await tools.shell({command:" + json.dumps("touch " + str(sentinel)) + "})"},
            }
            for name, arguments in cases.items():
                with self.subTest(name=name):
                    self.run_case(root, name, arguments, sentinel, original)

    def run_case(self, root, name, arguments, sentinel, original):
        requests = []
        unexpected = []
        class Provider(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args): pass
            def do_GET(self):
                unexpected.append(self.path)
                self.send_error(404)
            def do_POST(self):
                if self.path != "/v1/responses":
                    unexpected.append(self.path)
                    self.send_error(404)
                    return
                requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
                body = b"".join(b"data: " + json.dumps(e).encode() + b"\n\n" for e in response_events(name, arguments))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
        provider = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        base_url = "http://127.0.0.1:" + str(provider.server_port) + "/v1"
        if name == "webfetch": arguments["url"] = base_url + "/must-not-fetch"
        production_environment = BRIDGE.environment
        def loopback_environment(path, providers):
            env = production_environment(path, providers)
            config = json.loads(env["OPENCODE_CONFIG_CONTENT"])
            # The sole test override is transport. Every production agent, tool,
            # MCP, plugin, filesystem and registry setting remains identical.
            config["providers"]["openai"] = {"settings": {"baseURL": base_url}}
            expected = BRIDGE.config(providers)
            normalized = json.loads(json.dumps(config))
            normalized["providers"]["openai"] = {}
            self.assertEqual(normalized, expected)
            env["OPENCODE_CONFIG_CONTENT"] = json.dumps(config)
            return env
        server = None
        try:
            (Path(root) / name).mkdir()
            with patch.object(BRIDGE, "environment", loopback_environment):
                server = BRIDGE.Server(Path(root) / name, ["openai"], time.monotonic() + 15,
                                       binary=os.environ["PILLBOX_OPENCODE_TEST_BINARY"])
            info, models = server.ready(["openai"])
            runner_version = re.search(r"^ARG OPENCODE_VERSION=(\S+)$", (ROOT / "runner/Dockerfile").read_text(), re.MULTILINE)
            self.assertIsNotNone(runner_version, "runner pin not found")
            self.assertEqual(info["version"], runner_version.group(1), "native CLI differs from the resolved runner cohort")
            model = next(m for m in models if m["providerID"] == "openai" and m["id"] == "gpt-6-luna")
            # Fail before prompting if this exact catalog model does not resolve
            # to loopback, including any variant or request-body URL override.
            self.assertEqual(model["settings"]["baseURL"], base_url)
            variant = next(v for v in model["variants"] if v["id"] == "low")
            for settings in (model["settings"], variant.get("settings", {})):
                self.assertNotIn("baseURL", settings.get("body", {}))
            self.assertNotIn("baseURL", variant.get("settings", {}))
            with server.request("/api/event") as events:
                session = server.query("/api/session", {"title": "sealed text invocation", "agent": BRIDGE.AGENT,
                    "model": {"providerID": "openai", "id": "gpt-6-luna", "variant": "low"},
                    "permissions": BRIDGE.DENY})["data"]
                self.assertEqual(session["agent"], BRIDGE.AGENT)
                self.assertEqual(session["permissions"], BRIDGE.DENY)
                self.assertEqual(session["model"], {"providerID": "openai", "id": "gpt-6-luna", "variant": "low"})
                server.query("/api/session/" + session["id"] + "/prompt", {"text": "Refusal regression fixture."})
                attempted = False
                while True:
                    server.remaining()
                    line = events.readline(1024 * 1024)
                    self.assertTrue(line, "native stream closed before refusal")
                    if not line.startswith(b"data:"): continue
                    event = json.loads(line[5:])
                    data = event.get("data", {})
                    if data.get("sessionID") != session["id"] or data.get("id") != "call_fixture": continue
                    if event["type"] == "session.tool.input.started":
                        self.assertEqual(data["name"], name)
                        attempted = True
                    if event["type"] == "session.tool.success":
                        self.fail("a forced tool call succeeded")
                    if event["type"] == "session.tool.failed":
                        self.assertTrue(attempted, "failure did not follow the forced call")
                        self.assertEqual(data["error"], {"type": "tool.execution", "message":
                            'No tool named "' + name + '" is currently available. Please use a tool from the available tool list.'})
                        self.assertIs(data["executed"], False)
                        break
            # Native called.executed:false only denotes a local tool; it is
            # published before execution. The failed event above proves refusal.
            self.assertFalse(sentinel.exists(), "a file/shell tool executed")
            self.assertEqual(original.read_text(), "unchanged", "an edit tool executed")
            server.stop()
            server = None
            self.assertTrue(requests, "the fake provider was never called")
            self.assertTrue(all(not req.get("tools") for req in requests), "model was offered tools")
            self.assertTrue(all("tool_choice" not in req for req in requests), "model was given a tool choice")
            self.assertEqual(unexpected, [], "a network tool or unexpected provider route was used")
            self.assertFalse(sentinel.exists(), "a file/shell tool executed")
            self.assertEqual(original.read_text(), "unchanged", "an edit tool executed")
        finally:
            if server: server.stop()
            provider.shutdown()
            provider.server_close()
            thread.join(timeout=2)


if __name__ == "__main__":
    unittest.main()
