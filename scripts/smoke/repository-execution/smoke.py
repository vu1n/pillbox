#!/usr/bin/env python3
"""Prepare/inspect offline; explicit live mode runs exactly one synthetic coding turn."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import subprocess
import sys

from fingerprint_state import fingerprint
from inspect_completion import bounded_read, inspect_completion, load
from prepare import prepare, write_json


def binary_digest(path):
    assert path.is_file() and not path.is_symlink(), "binary must be a regular file"
    checksum = hashlib.sha256()
    with path.open("rb") as binary:
        for chunk in iter(lambda: binary.read(1024 * 1024), b""):
            checksum.update(chunk)
    return "sha256:" + checksum.hexdigest()


def capture(directory, name, argv, cwd, timeout=30, allowed=(0,)):
    """Keep outputs and actual status, including timeout/failure, before asserting."""
    env = dict(os.environ, PILLBOX_BACKEND="libkrun")
    record = {"argv": list(map(str, argv)), "cwd": str(cwd), "timeout_seconds": timeout}
    with (directory / (name + ".json")).open("xb") as stdout, (directory / (name + ".stderr")).open("xb") as stderr:
        try:
            result = subprocess.run(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                    stdout=stdout, stderr=stderr, timeout=timeout)
            record["exit_code"] = result.returncode
        except BaseException as error:
            record["error"] = str(error)
            write_json(directory, name + ".command.json", record)
            raise
    write_json(directory, name + ".command.json", record)
    assert allowed is None or result.returncode in allowed, "command failed; retained " + str(directory / (name + ".stderr"))
    return result.returncode


def runtime_identity(binary, root, directory):
    assert sys.platform == "darwin", "live mode currently requires the signed macOS libkrun binary"
    binary = binary.resolve(strict=True)
    capture(directory, "codesign-verify", ["/usr/bin/codesign", "--verify", "--strict", str(binary)], root)
    capture(directory, "codesign-entitlements", ["/usr/bin/codesign", "-d", "--entitlements", ":-", str(binary)], root)
    raw = bounded_read(directory / "codesign-entitlements.json")
    start = raw.find(b"<?xml")
    assert start >= 0, "signature did not return entitlements"
    entitlements = plistlib.loads(raw[start:])
    assert entitlements.get("com.apple.security.hypervisor") is True, "binary lacks Hypervisor entitlement"
    identity = {"path": str(binary), "sha256": binary_digest(binary)}
    write_json(directory, "binary.json", identity)
    capture(directory, "version", [str(binary), "--version"], root)
    capture(directory, "info", [str(binary), "info", "--json"], root)
    info = load(directory / "info.json")["pillbox"]
    assert info["scope"] == "project" and Path(info["source_dir"]).resolve() == root
    return binary, identity, Path(info["state_dir"])


def run_live(args):
    root = args.artifacts.resolve()
    prepare(root, args.image, args.attempt)
    directory = root / "observations" / "live"
    directory.mkdir(parents=True)  # Existing attempt, even failed, is never overwritten/relaunched.
    binary, identity, state = runtime_identity(args.binary, root, directory)
    request = load(root / "generated/request.json")
    identities = load(root / "generated/identities.json")
    capture(directory, "snapshot", [str(binary), "execution", "snapshot", "--repository", str(root / "repository"),
                                    "--commit", identities["commit"]], root)
    snapshot = load(directory / "snapshot.json")
    assert snapshot["snapshot_digest"] == identities["input_snapshot_digest"] and snapshot["files"] == 3
    write_json(directory, "state-before.json", fingerprint(state))
    assert binary_digest(binary) == identity["sha256"], "binary changed after signature verification"
    capture(directory, "execute", [str(binary), "execution", "execute", "--repository", str(root / "repository"),
                                   "--request", str(root / "generated/request.json")], root, timeout=660)
    capture(directory, "status", [str(binary), "execution", "status", request["invocation_id"]], root)
    assert load(directory / "execute.json")["execution"] == load(directory / "status.json")["execution"]
    checked = inspect_completion(root, directory / "execute.json", state)
    write_json(directory, "checked.json", checked)
    write_json(directory, "state-after.json", fingerprint(state))
    return checked


def verify_retry(args):
    root = args.artifacts.resolve()
    prepare(root, args.image, args.attempt)
    original = root / "observations/live"
    directory = root / "observations/retry"
    directory.mkdir()  # Explicit once-only proof, not an automatic recovery policy.
    binary, identity, state = runtime_identity(args.binary, root, directory)
    assert identity == load(original / "binary.json"), "retry binary differs from original execution"
    inspect_completion(root, original / "execute.json", state)
    before = fingerprint(state)
    write_json(directory, "state-before.json", before)
    argv = [str(binary), "execution", "execute", "--repository", str(root / "repository"), "--request"]
    capture(directory, "retry", argv + [str(root / "generated/request.json")], root)
    checked = inspect_completion(root, original / "execute.json", state, directory / "retry.json")
    after = fingerprint(state)
    write_json(directory, "state-after-retry.json", after)
    assert after == before, "identical retry changed state/evidence"
    status = capture(directory, "conflict", argv + [str(root / "generated/changed-retry.json")], root, allowed=None)
    after = fingerprint(state)
    write_json(directory, "state-after-conflict.json", after)
    assert status != 0 and after == before, "changed retry executed or changed state/evidence"
    error = bounded_read(directory / "conflict.stderr") + bounded_read(directory / "conflict.json")
    identities = load(root / "generated/identities.json")
    assert b"conflict" in error.lower()
    assert identities["request_hash"].encode() in error and identities["changed_request_hash"].encode() in error
    checked["changed_retry_conflicted_without_state_change"] = True
    write_json(directory, "checked.json", checked)
    return checked


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", nargs="?", default="prepare", choices=("prepare", "inspect", "fingerprint", "live", "verify-retry"))
    parser.add_argument("--artifacts", type=Path)
    parser.add_argument("--attempt", default="v1", help="explicit attempt identity; changing it never retries an existing invocation")
    parser.add_argument("--image", help="complete sha256 image ID; never a tag")
    parser.add_argument("--binary", type=Path, help="exact signed executable; required for explicit live/retry")
    parser.add_argument("--state-dir", type=Path)
    parser.add_argument("--completion", type=Path)
    parser.add_argument("--retry", type=Path)
    args = parser.parse_args()
    assert __debug__, "do not disable smoke assertions"
    required = {"prepare": ("artifacts", "image"), "inspect": ("artifacts", "completion", "state_dir"),
                "fingerprint": ("state_dir",), "live": ("artifacts", "image", "binary"),
                "verify-retry": ("artifacts", "image", "binary")}[args.mode]
    for key in required:
        if getattr(args, key) is None:
            parser.error("--" + key.replace("_", "-") + " is required for " + args.mode)
    if args.mode == "prepare":
        result = prepare(args.artifacts, args.image, args.attempt)
    elif args.mode == "inspect":
        result = inspect_completion(args.artifacts, args.completion, args.state_dir, args.retry)
    elif args.mode == "fingerprint":
        result = fingerprint(args.state_dir)
    else:
        result = run_live(args) if args.mode == "live" else verify_retry(args)
    print(json.dumps(result, sort_keys=True, indent=2))


if __name__ == "__main__":
    main()
