"""Sealed OpenCode 2 server bridge; also exports its offline image catalog.

No ambient config, credential DB, plugins or tool definitions cross this boundary.
The VM's host-owned vault is the only provider route; env values are stubs.
"""
import base64
import json
import os
import pathlib
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

AGENT = "pillbox_text"
DENY = [{"action": "*", "resource": "*", "effect": "deny"}]
PLUGINS = ["-*", "opencode.models.dev", "opencode.config.provider", "opencode.config.agent"]
ACTIVE_PLUGINS = set(PLUGINS[1:] + ["opencode.provider.opencode", "opencode.config.policy"])
PASSWORD = "pillbox-private-text-loopback"
RUNTIME = "/opt/pillbox-execution"


def config(providers):
    return {"update": "disable", "share": "disabled", "snapshots": False, "warming": False,
            "permissions": DENY, "default_agent": AGENT,
            "agents": {AGENT: {"mode": "primary", "permissions": DENY,
                               "system": "Answer the supplied input with final text only. Tools are forbidden."}},
            "plugins": PLUGINS, "providers": {p: {} for p in providers},
            "mcp": {"servers": {}}, "skills": [], "instructions": []}


def environment(root, providers):
    root = pathlib.Path(root)
    env = {"PATH": "/usr/local/bin:/usr/bin:/bin", "LANG": "C.UTF-8",
           "HOME": str(root / "home"), "TMPDIR": str(root / "tmp"),
           "OPENCODE_CONFIG_DIR": str(root / "config"),
           "OPENCODE_CONFIG_CONTENT": json.dumps(config(providers)),
           "OPENCODE_PASSWORD": PASSWORD, "OPENCODE_SERVER_PASSWORD": PASSWORD,
           "OPENCODE_DISABLE_MODELS_FETCH": "1", "OPENCODE_DISABLE_AUTOUPDATE": "1",
           "OPENCODE_DISABLE_PROJECT_CONFIG": "1", "OPENCODE_DISABLE_FILEWATCHER": "1",
           "OPENCODE_DISABLE_FFF": "1"}
    for kind in ("config", "data", "state", "cache"):
        env["XDG_" + kind.upper() + "_HOME"] = str(root / kind)
    for name in ("home", "tmp", "config", "data", "state", "cache", "work"):
        (root / name).mkdir(mode=0o700, exist_ok=True)
    return env


class Server:
    def __init__(self, root, providers, deadline, credentials=None, binary="/usr/local/bin/opencode"):
        self.deadline = deadline
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            self.port = sock.getsockname()[1]
        env = environment(root, providers)
        if credentials:
            env.update(credentials)
            env["NODE_EXTRA_CA_CERTS"] = RUNTIME + "/ca.crt"
            env["SSL_CERT_FILE"] = RUNTIME + "/ca.crt"
        self.proc = subprocess.Popen([binary, "serve", "--hostname", "127.0.0.1", "--port", str(self.port)],
                                     env=env, cwd=pathlib.Path(root) / "work", stdin=subprocess.DEVNULL,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                     close_fds=True, start_new_session=True)

    def remaining(self):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError("text deadline exceeded")
        return remaining

    def request(self, path, payload=None):
        headers = {"Authorization": "Basic " + base64.b64encode(("opencode:" + PASSWORD).encode()).decode()}
        if payload is not None:
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request("http://127.0.0.1:" + str(self.port) + path, headers=headers,
                                         data=None if payload is None else json.dumps(payload).encode())
        return urllib.request.urlopen(request, timeout=self.remaining() if path == "/api/event" else min(self.remaining(), 5))

    def query(self, path, payload=None):
        with self.request(path, payload) as response:
            body = response.read(4 * 1024 * 1024 + 1)
            if len(body) > 4 * 1024 * 1024:
                raise RuntimeError("API response limit exceeded")
            return json.loads(body)

    def ready(self, providers):
        while True:
            self.remaining()
            if self.proc.poll() is not None:
                raise RuntimeError("OpenCode exited before ready")
            try:
                info = self.query("/api/info")
                models = self.query("/api/model")["data"]
                agents = self.query("/api/agent")["data"]
                plugins = self.query("/api/plugin")["data"]
            except (OSError, urllib.error.URLError):
                time.sleep(min(0.05, self.remaining()))
                continue
            if not all(any(m["providerID"] == p for m in models) for p in providers) or not agents:
                time.sleep(min(0.05, self.remaining()))
                continue
            if len(agents) != 1 or agents[0]["id"] != AGENT or agents[0]["permissions"][-1] != DENY[0]:
                raise RuntimeError("sealed agent policy not installed")
            for plugin in plugins:
                if plugin["state"]["status"] == "active" and plugin["id"] not in ACTIVE_PLUGINS:
                    raise RuntimeError("unsealed plugin active")
            return info, models

    def stop(self):
        # The VM owner independently enforces the outer deadline and reap gate.
        if self.proc.poll() is None:
            os.killpg(self.proc.pid, 15)
            try:
                self.proc.wait(timeout=1)
            except subprocess.TimeoutExpired:
                os.killpg(self.proc.pid, 9)
                self.proc.wait(timeout=1)


def export_catalog(destination):
    with tempfile.TemporaryDirectory(prefix="pillbox-opencode-catalog-") as root:
        server = Server(root, ["anthropic", "openai"], time.monotonic() + 30)
        try:
            info, models = server.ready(["anthropic", "openai"])
            if not info["version"].startswith("2."):
                raise RuntimeError("unsupported OpenCode protocol")
            pathlib.Path(destination).write_text(json.dumps({"version": info["version"], "models": models}) + "\n")
        finally:
            server.stop()


def guest():
    with open(RUNTIME + "/limits.json", "rb") as handle:
        limits = json.load(handle)
    deadline = time.monotonic() + limits["duration_ms"] / 1000
    for argv in (["/usr/sbin/ip", "link", "set", "eth0", "up"],
                 ["/usr/sbin/ip", "addr", "add", "10.0.2.15/24", "dev", "eth0"],
                 ["/usr/sbin/ip", "route", "add", "default", "via", "10.0.2.2"]):
        subprocess.run(argv, env={"PATH": "/usr/sbin:/usr/bin:/bin"}, check=True,
                       stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                       timeout=min(5, max(0.001, deadline - time.monotonic())))
    pathlib.Path("/etc/resolv.conf").write_text("nameserver 10.0.2.2\n")
    with open(RUNTIME + "/credentials.json", "rb") as handle:
        credentials = json.load(handle)
    server = None
    with socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM) as sock:
        sock.settimeout(max(0.001, deadline - time.monotonic()))
        sock.connect((2, 1067))
        channel = sock.makefile("rb")
        request_line = channel.readline(1024 * 1024 + 1)
        if len(request_line) > 1024 * 1024 or not request_line.endswith(b"\n"):
            raise RuntimeError("invalid text request frame")
        request = json.loads(request_line)
        provider = request["model"]["providerID"]
        server = Server("/home/pillbox", [provider], deadline, credentials)
        total = 0

        def emit(event):
            nonlocal total
            frame = json.dumps(event, separators=(",", ":")).encode() + b"\n"
            total += len(frame)
            if len(frame) - 1 > request["max_frame_bytes"] or total > request["max_evidence_bytes"]:
                raise RuntimeError("native frame or evidence limit exceeded")
            sock.sendall(frame)

        try:
            info, models = server.ready([provider])
            if not any(m["providerID"] == provider and m["id"] == request["model"]["id"] for m in models):
                raise RuntimeError("resolved model absent in guest")
            emit({"type": "pillbox.info", "version": info["version"]})
            # Subscribe before creating the session or admitting any prompt.
            with server.request("/api/event") as events:
                session = server.query("/api/session", {"title": "sealed text invocation", "agent": AGENT,
                                       "model": request["model"], "permissions": DENY})["data"]
                if session["agent"] != AGENT or session["model"] != request["model"] or session["permissions"] != DENY:
                    raise RuntimeError("session policy mismatch")
                emit({"type": "pillbox.session", "id": session["id"]})
                server.query("/api/session/" + session["id"] + "/prompt", {"text": request["prompt"]})
                data = []
                size = 0
                while True:
                    server.remaining()
                    line = events.readline(request["max_frame_bytes"] + 2)
                    if not line:
                        raise RuntimeError("SSE closed before terminal event")
                    size += len(line)
                    if size > request["max_frame_bytes"]:
                        raise RuntimeError("SSE frame limit exceeded")
                    if line.strip() == b"":
                        if data:
                            event = json.loads(b"\n".join(data))
                            emit(event)
                            if event["type"] in ("session.execution.succeeded", "session.execution.failed", "session.execution.interrupted"):
                                # Keep the VM alive until the host consumes the terminal
                                # frame and closes its channel; process exit is not an ACK.
                                channel.read(1)
                                return
                        data, size = [], 0
                    elif line.startswith(b"data:"):
                        data.append(line[5:].strip())
        finally:
            if server:
                server.stop()


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--export-image-metadata":
        export_catalog(sys.argv[2])
    elif len(sys.argv) == 1:
        guest()
    else:
        raise RuntimeError("unsupported bridge mode")
