#!/usr/bin/env python3
"""import-swebench.py — SWE-bench(-Lite) instances → pillbox frozen-task format (the in-repo slice).

The majority of real use is IN-REPO edits (the agent sees a real codebase and changes it), not
greenfield function-from-a-stub. This imports SWE-bench-style tasks onto our existing frozen-task
layout so the same harness (freeze-task / gate / ghost) runs them unchanged:
  workspace/  = the repo at base_commit, .git stripped — the agent's starting tree (ALL it sees;
                no history, so it can't `git log` the gold fix)
  grader/     = the hidden test_patch (applied at grade time) + grade.sh (apply → run FAIL_TO_PASS)
                + the FAIL_TO_PASS / PASS_TO_PASS node lists. Never copied in until grade time.
  prompt.txt  = problem_statement (the issue; no tests leaked)
  meta.json   = repo, base_commit, environment_setup_commit, version

ENVIRONMENT — shaped to pillbox, not fought: grading runs IN-SANDBOX (`session score --in-sandbox`
+ `--grader-egress` to pypi) so the repo's deps install in the runner microVM with pypi reachable.
v1 installs at grade time (dep-light repos first). The amortization — materialize a per-repo
env-base ONCE, `pillbox push` it, and have each task's grade build on that frozen env (rustic dedups
the repo+env across tasks) — is the next step (the right pillbox-native fix); start simple here.

Data via the HF datasets-server (curl + JSON; no `datasets` lib). Repos cloned + cached under
.cache/swebench-repos/. Filter to dep-light repos for the first proof (e.g. --repo flask).

Usage: import-swebench.py [--dataset princeton-nlp/SWE-bench_Lite] [--repo NAME] [--limit N]
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import urllib.request

DS_SERVER = "https://datasets-server.huggingface.co/rows"
GIT_BASE = "https://github.com"

# pytest is the runner for nearly all SWE-bench repos; FAIL_TO_PASS/PASS_TO_PASS are pytest node ids.
GRADE_SH = """#!/bin/sh
# Hidden grader (injected at grade time, run --in-sandbox with --grader-egress pypi.org
# files.pythonhosted.org). Installs the repo + test deps, applies the hidden test patch, then runs
# the FAIL_TO_PASS tests. Exit 0 iff they all pass.
set -e
pip install -e . >/dev/null 2>&1 || pip install . >/dev/null 2>&1 || true
pip install pytest >/dev/null 2>&1 || true
git apply --3way test_patch.diff 2>/dev/null || patch -p1 < test_patch.diff
exec python -m pytest -q --no-header {fail_to_pass}
"""


def fetch_rows(dataset: str, split: str, offset: int, length: int) -> list[dict]:
    url = f"{DS_SERVER}?dataset={dataset}&config=default&split={split}&offset={offset}&length={length}"
    with urllib.request.urlopen(url, timeout=60) as r:
        return [row["row"] for row in json.load(r)["rows"]]


def clone_at(repo: str, base_commit: str, cache: str) -> str:
    """Clone `org/name` (cached) and check out base_commit; return the checkout path."""
    repo_dir = os.path.join(cache, repo.replace("/", "__"))
    if not os.path.isdir(os.path.join(repo_dir, ".git")):
        os.makedirs(os.path.dirname(repo_dir), exist_ok=True)
        subprocess.run(["git", "clone", "--quiet", f"{GIT_BASE}/{repo}.git", repo_dir], check=True)
    subprocess.run(["git", "-C", repo_dir, "fetch", "--quiet", "origin", base_commit], check=False)
    subprocess.run(["git", "-C", repo_dir, "checkout", "--quiet", "--force", base_commit], check=True)
    subprocess.run(["git", "-C", repo_dir, "clean", "-qxdf"], check=False)
    return repo_dir


def build_task(inst: dict, repo_dir: str, out_dir: str):
    """Materialize one frozen-task dir from a SWE-bench instance + its checked-out repo."""
    if os.path.isdir(out_dir):
        shutil.rmtree(out_dir)
    ws, grader = os.path.join(out_dir, "workspace"), os.path.join(out_dir, "grader")
    # workspace = repo source at base_commit, WITHOUT .git (no history → no gold-fix leakage).
    shutil.copytree(repo_dir, ws, ignore=shutil.ignore_patterns(".git"))
    os.makedirs(grader)
    with open(os.path.join(grader, "test_patch.diff"), "w") as f:
        f.write(inst["test_patch"])
    f2p = inst["FAIL_TO_PASS"]
    f2p = json.loads(f2p) if isinstance(f2p, str) else f2p
    nodes = " ".join(_shq(n) for n in f2p)
    with open(os.path.join(grader, "grade.sh"), "w") as f:
        f.write(GRADE_SH.format(fail_to_pass=nodes))
    with open(os.path.join(grader, "fail_to_pass.json"), "w") as f:
        json.dump(f2p, f, indent=2)
    with open(os.path.join(out_dir, "prompt.txt"), "w") as f:
        f.write(inst["problem_statement"])
    with open(os.path.join(out_dir, "meta.json"), "w") as f:
        json.dump({k: inst.get(k) for k in
                   ("instance_id", "repo", "base_commit", "environment_setup_commit", "version")},
                  f, indent=2)


def _shq(s: str) -> str:
    return "'" + s.replace("'", "'\\''") + "'"


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    ap = argparse.ArgumentParser()
    ap.add_argument("--dataset", default="princeton-nlp/SWE-bench_Lite")
    ap.add_argument("--split", default="test")
    ap.add_argument("--repo", default="", help="substring filter on repo (e.g. flask) — dep-light first")
    ap.add_argument("--limit", type=int, default=3)
    ap.add_argument("--out", default=os.path.join(here, "tasks"))
    ap.add_argument("--cache", default=os.path.join(here, ".cache", "swebench-repos"))
    args = ap.parse_args()

    made, scanned, offset = 0, 0, 0
    while made < args.limit and scanned < 300:
        rows = fetch_rows(args.dataset, args.split, offset, 50)
        if not rows:
            break
        offset += len(rows)
        for inst in rows:
            scanned += 1
            if args.repo and args.repo.lower() not in inst["repo"].lower():
                continue
            tid = "swe_" + inst["instance_id"].replace("__", "_").replace("-", "_")
            try:
                repo_dir = clone_at(inst["repo"], inst["base_commit"], args.cache)
                build_task(inst, repo_dir, os.path.join(args.out, tid))
                print(f"{tid}: {inst['repo']}@{inst['base_commit'][:8]} "
                      f"({len(json.loads(inst['FAIL_TO_PASS']) if isinstance(inst['FAIL_TO_PASS'], str) else inst['FAIL_TO_PASS'])} fail_to_pass)")
                made += 1
            except Exception as e:
                print(f"skip {inst['instance_id']}: {e}", file=sys.stderr)
            if made >= args.limit:
                break
    print(f"\nimported {made} in-repo task(s) → {args.out}")
    print("grade with: session score --in-sandbox --grader-egress pypi.org --grader-egress files.pythonhosted.org")


if __name__ == "__main__":
    main()
