#!/usr/bin/env python3
"""Read only: verify captured CLI output and its actual local evidence bytes."""
import argparse
import base64
import json
from pathlib import Path
import re
import stat
import tempfile

from prepare import FILES, canonical_digest, digest, git

def load(path):
    return json.loads(bounded_read(Path(path)))

def bounded_read(path, maximum=64 * 1024 * 1024):
    assert path.is_file() and not path.is_symlink(), "missing/nonregular evidence: " + str(path)
    with path.open("rb") as handle:
        data = handle.read(maximum + 1)
    assert len(data) <= maximum, "evidence too large"
    return data

def inspect_completion(artifacts, completion_path, state_dir, retry=None):
    out = artifacts / "generated"
    record = load(completion_path)["execution"]
    request = load(out / "request.json")
    identities = load(out / "identities.json")
    assert canonical_digest(request) == identities["request_hash"]
    assert canonical_digest(request["manifest"]) == identities["manifest_digest"]
    assert canonical_digest(request["manifest"]["verifier"]["definition"]) == request["manifest"]["verifier"]["definition_digest"]
    assert record["invocation_id"] == request["invocation_id"] and record["status"] == "completed"
    assert record["request_hash"] == identities["request_hash"]
    if retry:
        assert load(retry)["execution"] == record, "identical retry changed durable record"
    completion = record["detail"]
    admission, result, verification = (completion[k] for k in ("admission", "result", "verification"))
    for receipt in [admission, result]:
        assert receipt["invocation_id"] == request["invocation_id"]
        assert receipt["request_hash"] == identities["request_hash"]
        assert receipt["manifest_digest"] == identities["manifest_digest"]
    assert admission["runner_image_id"] == identities["image_id"]
    assert admission["input_snapshot_digest"] == identities["input_snapshot_digest"]
    assert result["base"] == request["manifest"]["base"]
    assert result["execution"] == request["execution"]
    assert result["output_id"] == request["manifest"]["output_id"]
    assert result["changed_paths"] == ["src/answer.txt"]
    assert result["result_snapshot_digest"] == identities["expected_result_snapshot_digest"]
    sealed = request["manifest"]["verifier"]
    for key in ["verifier_id", "run_id", "definition_digest"]:
        assert verification[key] == sealed[key]
    assert verification["output_id"] == result["output_id"]
    assert verification["result_digest"] == canonical_digest(result)
    assert verification["result_snapshot_digest"] == result["result_snapshot_digest"]
    assert verification["outcome"] == "pass"

    def session_path(identity):
        assert re.fullmatch(r"[A-Za-z0-9_-]{1,128}", identity)
        return state_dir / "sessions" / identity

    def evidence(ref):
        low, high = ref["seq_range"]
        assert type(low) is int and type(high) is int and 1 <= low <= high
        events = [json.loads(line) for line in bounded_read(session_path(ref["session_id"]) / "log.jsonl").splitlines()]
        positions = [event["seq"] for event in events if low <= event["seq"] <= high]
        assert positions == list(range(low, high + 1)), "evidence range missing positions"
        return events

    for receipt in [admission, result, verification]:
        evidence(receipt["evidence"])
    assert result["evidence"]["session_id"] == request["session_ref"]["session_id"]
    assert verification["evidence"]["session_id"] != result["evidence"]["session_id"]
    for ref in (result["snapshot_manifest"], result["patch"], completion["native_evidence"], completion["text"]):
        assert ref["session_id"] == result["evidence"]["session_id"]
    assert verification["report"]["session_id"] == verification["evidence"]["session_id"]

    def artifact(ref):
        assert re.fullmatch(r"sha256:[0-9a-f]{64}", ref["digest"])
        data = bounded_read(session_path(ref["session_id"]) / "blobs" / ref["digest"][7:])
        assert len(data) == ref["bytes"] and digest(data) == ref["digest"]
        return data

    snapshot_bytes = artifact(result["snapshot_manifest"])
    snapshot = json.loads(snapshot_bytes)
    assert snapshot == load(out / "expected-result-manifest.json")
    assert digest(snapshot_bytes) == result["result_snapshot_digest"]
    expected_files = {**FILES, "src/answer.txt": b"answer=42\n"}
    for entry in snapshot:
        data = bounded_read(session_path(result["snapshot_manifest"]["session_id"]) / "blobs" / entry["sha256"][7:])
        assert digest(data) == entry["sha256"] and data == expected_files[entry["path"]]
    patch = artifact(result["patch"])
    assert patch and len(patch) <= request["manifest"]["limits"]["max_patch_bytes"]
    reproduce_patch(artifacts / "repository", identities["commit"], patch, snapshot)
    report_bytes = artifact(verification["report"])
    assert len(report_bytes) <= 4 * ((sealed["definition"]["max_output_bytes"] + 2) // 3) + 4096
    assert report_bytes.endswith(b"\n") and report_bytes.count(b"\n") == 1
    report = json.loads(report_bytes)
    assert report["version"] == "pillbox.verifier/1"
    for key in ["verifier_id", "run_id", "definition_digest", "output_id", "result_digest", "result_snapshot_digest"]:
        assert report[key] == verification[key]
    assert type(report["exit_code"]) is int and report["exit_code"] == 0 and report["signal"] is None
    assert report["timed_out"] is False and report["output_limited"] is False
    assert len(base64.b64decode(report["stdout_base64"], validate=True)) + len(base64.b64decode(report["stderr_base64"], validate=True)) <= sealed["definition"]["max_output_bytes"]
    native = artifact(completion["native_evidence"])
    assert len(native) <= request["manifest"]["limits"]["max_evidence_bytes"]
    frames = [json.loads(line) for line in native.splitlines()]
    turns = [f["message"] for f in frames if f["direction"] == "outbound" and f["message"].get("method") == "turn/start"]
    responses = [f["message"]["result"] for f in frames if f["direction"] == "inbound" and f["message"].get("id") == "pillbox-thread"]
    assert len(responses) == 1
    observed = responses[0]
    assert observed["model"] == "gpt-5.6-sol" and observed["reasoningEffort"] == "low"
    assert observed["modelProvider"] == "pillbox_openai_http" and observed["thread"]["cliVersion"] == "0.151.0"
    thread_id = observed["thread"]["id"]
    terminal = [f["message"]["params"] for f in frames if f["direction"] == "inbound" and f["message"].get("method") == "turn/completed"]
    assert len(terminal) == 1 and terminal[0]["threadId"] == thread_id
    assert terminal[0]["turn"]["status"] == "completed" and terminal[0]["turn"]["error"] is None
    turn_id = terminal[0]["turn"]["id"]
    assert not any(f["message"].get("method") == "model/rerouted" for f in frames)
    assert len(turns) == 1 and turns[0]["params"]["model"] == "gpt-5.6-sol" and turns[0]["params"]["effort"] == "low"
    assert turns[0]["params"]["input"][0]["text"] == request["rendered_input"]
    calls = [f["message"]["params"] for f in frames if f["direction"] == "inbound" and f["message"].get("method") == "item/tool/call"]
    assert 3 <= len(calls) <= 10
    read_paths = set()
    writes = 0
    assert len({call["callId"] for call in calls}) == len(calls)
    for call in calls:
        assert call["threadId"] == thread_id and call["turnId"] == turn_id
        arguments = call["arguments"]
        if call["tool"] == "pillbox_read_file":
            assert arguments["path"] in request["manifest"]["scope"]["read_paths"]
            read_paths.add(arguments["path"])
        else:
            assert call["tool"] == "pillbox_write_file" and arguments["path"] == "src/answer.txt"
            assert arguments["executable"] is False
            assert arguments["encoding"] in ("utf8", "base64")
            content = arguments["content"].encode("utf-8") if arguments["encoding"] == "utf8" else base64.b64decode(arguments["content"], validate=True)
            assert content == b"answer=42\n"
            writes += 1
    assert read_paths == {"docs/task.txt", "src/answer.txt"} and writes >= 1
    assert FILES["private/ungranted.txt"].strip() not in native
    assert base64.b64encode(FILES["private/ungranted.txt"]) not in native
    artifact(completion["text"])
    return {"captured_bytes_checked": True, "patch_reproduces_complete_snapshot": True, "native_turn_start_count": len(turns), "tool_calls": len(calls),
                      "changed_paths": result["changed_paths"], "request_hash": record["request_hash"],
                      "result_digest": verification["result_digest"], "result_snapshot_digest": result["result_snapshot_digest"],
                      "builder_evidence": result["evidence"], "verifier_evidence": verification["evidence"],
                      "retry_record_equal": bool(retry), "verification": "pass"}

def reproduce_patch(repository, commit, patch, expected_manifest):
    assert git(repository, "ls-tree", "-r", "--name-only", commit).decode().splitlines() == sorted(FILES)
    with tempfile.TemporaryDirectory(prefix="pillbox-patch-check-") as name:
        tree = Path(name)
        for path, expected in FILES.items():
            content = git(repository, "show", commit + ":" + path)
            assert content == expected, "committed smoke base changed"
            target = tree / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(content)
            target.chmod(0o600)
        git(tree, "apply", "--binary", "--whitespace=nowarn", "-p1", "-", input=patch)
        actual = []
        for path in sorted(tree.rglob("*")):
            mode = path.lstat().st_mode
            assert stat.S_ISDIR(mode) or stat.S_ISREG(mode), "patch created a nonregular entry"
            if stat.S_ISREG(mode):
                actual.append({"executable": bool(mode & 0o111), "path": path.relative_to(tree).as_posix(),
                               "sha256": digest(bounded_read(path))})
        assert actual == expected_manifest, "patch does not reproduce the complete result snapshot"

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--completion", type=Path, required=True)
    parser.add_argument("--state-dir", type=Path, required=True)
    parser.add_argument("--retry", type=Path)
    args = parser.parse_args()
    assert __debug__, "do not disable smoke assertions"
    print(json.dumps(inspect_completion(args.artifacts, args.completion, args.state_dir, args.retry), indent=2))
