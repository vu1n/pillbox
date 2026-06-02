#!/usr/bin/env python3
"""Import the Aider polyglot benchmark (Python track) into the eval rig.

Aider-polyglot is Exercism-derived: each exercise declares its editable solution
file(s) and test file(s) in .meta/config.json. We map them onto the hidden-grader
layout — solution files the agent edits, test files it never sees:

  tasks/ap_<exercise>/
    workspace/<solution>.py   the stub (pass-bodies) the agent implements
    grader/<test>_test.py      the HIDDEN unittest (injected at grade time)
    grader/grade.sh            python3 -m unittest discover (exit 0 = pass)
    prompt.txt                 the exercise instructions (no test leaked)

Less contaminated + more agentic than HumanEval, and graded with stdlib unittest
on the host (no per-task env) — the better A/B set for the memory loop. Python
only (other langs need their toolchains on the host).

Usage: import-aider-polyglot.py [--limit N] [--repo PATH] [--out DIR]
  --repo : a checkout of Aider-AI/polyglot-benchmark (default: clone to .cache/)
"""
import argparse
import json
import os
import shutil
import stat
import subprocess

REPO_URL = "https://github.com/Aider-AI/polyglot-benchmark"
GRADE_SH = "#!/bin/sh\npython3 -m unittest discover -p '*_test.py' 2>&1\n"


def main() -> None:
    here = os.path.dirname(os.path.abspath(__file__))
    ap = argparse.ArgumentParser()
    ap.add_argument("--limit", type=int, default=0, help="first N exercises (0 = all)")
    ap.add_argument("--repo", default="", help="polyglot-benchmark checkout (else clone to .cache)")
    ap.add_argument("--out", default=os.path.join(here, "tasks"))
    args = ap.parse_args()

    repo = args.repo or _ensure_clone(os.path.join(here, ".cache", "polyglot-benchmark"))
    practice = os.path.join(repo, "python", "exercises", "practice")
    names = sorted(d for d in os.listdir(practice) if os.path.isdir(os.path.join(practice, d)))
    if args.limit:
        names = names[: args.limit]

    n = 0
    for name in names:
        ex = os.path.join(practice, name)
        cfg_path = os.path.join(ex, ".meta", "config.json")
        if not os.path.exists(cfg_path):
            continue
        with open(cfg_path) as f:
            files = json.load(f).get("files", {})
        solution, test = files.get("solution", []), files.get("test", [])
        if not solution or not test:
            continue  # malformed exercise; skip rather than emit a broken task

        d = os.path.join(args.out, "ap_" + name.replace("-", "_"))
        ws, gr = os.path.join(d, "workspace"), os.path.join(d, "grader")
        os.makedirs(ws, exist_ok=True)
        os.makedirs(gr, exist_ok=True)
        for rel in solution:
            shutil.copy(os.path.join(ex, rel), os.path.join(ws, os.path.basename(rel)))
        for rel in test:
            shutil.copy(os.path.join(ex, rel), os.path.join(gr, os.path.basename(rel)))
        _write(os.path.join(gr, "grade.sh"), GRADE_SH)
        _write(os.path.join(d, "prompt.txt"), _prompt(ex, solution))
        n += 1

    print(f"imported {n} Aider-polyglot Python exercise(s) → {args.out}")


def _prompt(ex: str, solution: list) -> str:
    parts = []
    for doc in ("instructions.md", "instructions.append.md"):
        p = os.path.join(ex, ".docs", doc)
        if os.path.exists(p):
            with open(p) as f:
                parts.append(f.read().strip())
    body = "\n\n".join(parts)
    files = ", ".join(os.path.basename(s) for s in solution)
    return f"{body}\n\nImplement the solution in {files} to pass the tests. Edit those file(s) directly.\n"


def _ensure_clone(dest: str) -> str:
    if not os.path.isdir(os.path.join(dest, ".git")):
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        subprocess.run(["git", "clone", "--depth", "1", REPO_URL, dest], check=True)
    return dest


def _write(path: str, content: str) -> None:
    with open(path, "w") as f:
        f.write(content)
    if path.endswith(".sh"):
        os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC)


if __name__ == "__main__":
    main()
