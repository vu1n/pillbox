"""Offline tests of the exact bridge/config exported into the microVM image."""
import functools
import importlib.util
import json
import io
import os
import socket
import threading
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
SPEC = importlib.util.spec_from_file_location("opencode_bridge", ROOT / "src/execution/opencode_bridge.py")
BRIDGE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BRIDGE)


class PolicyTests(unittest.TestCase):
    def test_guest_terminal_waits_for_host_consumption_before_shutdown(self):
        host, guest = socket.socketpair()
        host.settimeout(2)
        finished = threading.Event()
        failures = []
        class Channel:
            def __enter__(self): return self
            def __exit__(self, *args): guest.close()
            def settimeout(self, value): guest.settimeout(value)
            def connect(self, address): pass
            def makefile(self, mode): return guest.makefile(mode)
            def sendall(self, data): guest.sendall(data)
        class FakeServer:
            def __init__(self, *args): pass
            def ready(self, providers):
                return {"version": "2.0.24"}, [{"providerID": "openai", "id": "fixture"}]
            def remaining(self): return 2
            def request(self, path):
                return io.BytesIO(b'data: {"type":"session.execution.succeeded","data":{"sessionID":"s"}}\n\n')
            def query(self, path, payload):
                return {"data": {**payload, "id": "s"}}
            def stop(self): finished.set()
        def run():
            try: BRIDGE.guest()
            except BaseException as error: failures.append(error)
        with tempfile.TemporaryDirectory() as root:
            Path(root, "limits.json").write_text(json.dumps({"duration_ms": 2000}))
            Path(root, "credentials.json").write_text(json.dumps({"OPENAI_API_KEY": "fixture-stub"}))
            with patch.object(BRIDGE, "RUNTIME", root), patch.object(BRIDGE, "Server", FakeServer), \
                 patch.object(BRIDGE.subprocess, "run"), patch.object(BRIDGE.socket, "socket", lambda *args: Channel()), \
                 patch.object(BRIDGE.pathlib.Path, "write_text"):
                thread = threading.Thread(target=run)
                thread.start()
                host.sendall(json.dumps({"model": {"providerID": "openai", "id": "fixture"},
                    "prompt": "fixture", "max_frame_bytes": 4096, "max_evidence_bytes": 8192}).encode() + b"\n")
                with host.makefile("rb") as stream:
                    for _ in range(3): self.assertTrue(stream.readline())
                    self.assertFalse(finished.is_set(), "guest stopped before host consumed terminal and disconnected")
                    host.shutdown(socket.SHUT_RDWR)
                host.close()
                thread.join(timeout=2)
                self.assertFalse(thread.is_alive())
                self.assertTrue(finished.is_set())
                self.assertEqual(failures, [])

    def test_every_tool_class_has_last_matching_deny_and_no_loader(self):
        config = BRIDGE.config(["openai"])
        for action in ("read", "write", "edit", "shell", "webfetch", "execute", "mcp_private_tool"):
            for rules in (config["permissions"], config["agents"][BRIDGE.AGENT]["permissions"]):
                matching = [rule for rule in rules if rule["action"] in ("*", action)]
                self.assertEqual(matching[-1], {"action": "*", "resource": "*", "effect": "deny"})
        self.assertEqual(config["mcp"], {"servers": {}})
        self.assertEqual(config["plugins"], ["-*", "opencode.models.dev", "opencode.config.provider", "opencode.config.agent"])
        self.assertNotIn("steps", config["agents"][BRIDGE.AGENT])

    def test_environment_does_not_inherit_credentials_or_config(self):
        with tempfile.TemporaryDirectory() as root:
            with patch.dict(os.environ, {"OPENAI_API_KEY": "test-only-parent-key", "OPENCODE_CONFIG": "/ambient.json"}):
                env = BRIDGE.environment(root, ["openai"])
            self.assertNotIn("OPENAI_API_KEY", env)
            self.assertNotIn("OPENCODE_CONFIG", env)
            self.assertEqual(env["OPENCODE_DISABLE_MODELS_FETCH"], "1")
            self.assertEqual(env["OPENCODE_DISABLE_PROJECT_CONFIG"], "1")
            self.assertEqual(env["OPENCODE_DISABLE_AUTOUPDATE"], "1")
            self.assertEqual(json.loads(env["OPENCODE_CONFIG_CONTENT"])["providers"], {"openai": {}})

    def test_unsealed_agent_or_external_plugin_fails_before_prompt(self):
        server = object.__new__(BRIDGE.Server)
        server.proc = type("Process", (), {"poll": lambda _: None})()
        server.remaining = lambda: 10
        replies = {"/api/info": {"version": "2.0.24"},
                   "/api/model": {"data": [{"providerID": "openai"}]},
                   "/api/agent": {"data": [{"id": BRIDGE.AGENT, "permissions": BRIDGE.DENY}]},
                   "/api/plugin": {"data": [{"id": "ambient-plugin", "state": {"status": "active"}}]}}
        server.query = lambda path: replies[path]
        with self.assertRaisesRegex(RuntimeError, "unsealed plugin"):
            server.ready(["openai"])
        replies["/api/plugin"]["data"] = []
        replies["/api/agent"]["data"][0]["permissions"] = [{"action": "*", "resource": "*", "effect": "allow"}]
        with self.assertRaisesRegex(RuntimeError, "policy not installed"):
            server.ready(["openai"])


class NativeCatalogTests(unittest.TestCase):
    @unittest.skipUnless(os.environ.get("PILLBOX_OPENCODE_TEST_BINARY"), "optional credential-free native CLI probe")
    def test_bundled_catalog_export_and_deny_policy_on_native_server(self):
        binary = os.environ["PILLBOX_OPENCODE_TEST_BINARY"]
        original = BRIDGE.Server
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "catalog.json"
            with patch.object(BRIDGE, "Server", functools.partial(original, binary=binary)):
                BRIDGE.export_catalog(path)
            catalog = json.loads(path.read_text())
        self.assertTrue(catalog["version"].startswith("2."))
        for provider in ("openai", "anthropic"):
            self.assertTrue(any(m["providerID"] == provider and m["enabled"] for m in catalog["models"]))


if __name__ == "__main__":
    unittest.main()
