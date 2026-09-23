/** Detached bounded worker: completion is a native HTTP response, never CLI process exit. */
export const DIGITALOCEAN_GUEST = String.raw`
import json, os, signal, subprocess, sys, tempfile, time, urllib.request
payload = json.loads(sys.argv[1])
result_path = sys.argv[2]
server = None
result = {"error": "runtime_failed"}
phase = "harness_check"
def deadline(signum, frame):
    raise TimeoutError("native_turn_deadline")
signal.signal(signal.SIGALRM, deadline)
signal.alarm(260)
try:
    version = subprocess.run(["opencode", "--version"], capture_output=True, text=True, timeout=10)
    if version.returncode != 0 or version.stdout.strip() != payload["harness_version"]:
        raise RuntimeError("harness_version_mismatch")
    phase = "authentication_check"
    if not os.environ.get("OPENROUTER_API_KEY"):
        raise RuntimeError("auth_unavailable")
    phase = "server_start"
    directory = tempfile.mkdtemp(prefix="pillbox-turn-")
    env = {k: v for k, v in os.environ.items() if k in ["PATH", "HOME", "OPENROUTER_API_KEY"]}
    for key in ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME"]:
        env[key] = os.path.join(directory, key)
    env["OPENCODE_CONFIG_CONTENT"] = json.dumps({
        "permission": "deny", "share": "disabled", "autoupdate": False,
        "plugin": [], "mcp": {}, "enabled_providers": ["openrouter"],
        "provider": {"openrouter": {
            "options": {"apiKey": "{env:OPENROUTER_API_KEY}"},
            "models": {payload["model"]: {"variants": {
                payload["reasoning"]["variant"]: payload["reasoning"]["options"]
            }}}
        }}
    })
    server = subprocess.Popen(["opencode", "serve", "--port", "4197", "--hostname", "127.0.0.1"],
        cwd=directory, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    def request(path, body=None, timeout=5):
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request("http://127.0.0.1:4197" + path, data=data,
            headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=timeout) as response:
            raw = response.read(98305)
            if len(raw) > 98304:
                raise RuntimeError("response_too_large")
            return json.loads(raw)
    ready = False
    for attempt in range(20):
        try:
            request("/global/health")
            ready = True
            break
        except (OSError, ValueError):
            if server.poll() is not None:
                break
            time.sleep(0.5)
    if not ready:
        raise RuntimeError("server_unavailable")
    phase = "session_create"
    session = request("/session", {"permission": [{"permission": "*", "pattern": "*", "action": "deny"}]})
    body = {"parts": [{"type": "text", "text": payload["text"]}],
        "model": {"providerID": "openrouter", "modelID": payload["model"]},
        "tools": payload["tools"], "variant": payload["reasoning"]["variant"]}
    if payload["output_format"]["type"] == "json_schema":
        body["format"] = {"type": "json_schema", "schema": payload["output_format"]["schema"], "retryCount": 2}
    phase = "native_turn"
    native = request("/session/" + session["id"] + "/message", body, timeout=230)
    phase = "native_result_validation"
    info = native.get("info", {})
    native_error = info.get("error")
    # OpenCode can return schema-valid text with StructuredOutputError when its response
    # tool is unavailable. Keep CF parity: the trusted adapter must validate that text.
    structured_missing = (payload["output_format"]["type"] == "json_schema"
        and isinstance(native_error, dict) and native_error.get("name") == "StructuredOutputError")
    if (native_error and not structured_missing) or not info.get("time", {}).get("completed"):
        raise RuntimeError("incomplete_turn")
    parts = native.get("parts", [])
    if any(p.get("type") == "tool" and p.get("tool") != "StructuredOutput" for p in parts):
        raise RuntimeError("unexpected_tool")
    result = {"info": {k: info[k] for k in ["id", "role", "providerID", "modelID", "finish", "tokens", "cost", "structured"] if k in info},
        "text": "".join(p.get("text", "") for p in parts if p.get("type") == "text"),
        "harness_version": version.stdout.strip()}
except Exception:
    # Provider errors can contain prompts, keys or URLs. The trusted adapter reports only this code.
    result = {"error": phase}
finally:
    signal.alarm(0)
    if server is not None:
        try:
            os.killpg(server.pid, signal.SIGKILL)
            server.wait(timeout=5)
        except ProcessLookupError:
            pass
    encoded = json.dumps(result)
    if len(encoded.encode()) > 98304:
        encoded = json.dumps({"error": "result_too_large"})
    with open(result_path + ".partial", "x") as output:
        output.write(encoded)
    os.replace(result_path + ".partial", result_path)
`;

export const DIGITALOCEAN_LAUNCH = String.raw`
import os, subprocess, sys
path = sys.argv[3]
# Exclusive creation makes an accidentally repeated launch fail before sampling.
fd = os.open(path + ".started", os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
os.close(fd)
subprocess.Popen([sys.executable, "-c", sys.argv[1], sys.argv[2], path],
    stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
print("started")
`;

export const DIGITALOCEAN_POLL = String.raw`
import os, sys
path = sys.argv[1]
if not os.path.exists(path):
    print('{"pending":true}')
else:
    with open(path, "rb") as source:
        data = source.read(98305)
    if len(data) > 98304:
        raise RuntimeError("result_too_large")
    print(data.decode())
`;
