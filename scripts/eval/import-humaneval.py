#!/usr/bin/env python3
"""Import HumanEval into the eval rig as one task dir per problem.

Each problem → tasks/he_<id>/
  workspace/solution.py   the prompt stub (signature + docstring) the agent completes
  grader/check.py         the HIDDEN test (injected into the clone only at grade time)
  grader/grade.sh         runs check.py (exit 0 = pass)
  prompt.txt              the instruction (no test leaked)

The agent never sees grader/ (run-task.sh copies only workspace/ into the
sandbox), so it can't read the test and hardcode.

Usage: import-humaneval.py [--limit N] [--out DIR] [--url URL]
  --limit N : import only the first N problems (default: all 164)
  --out DIR : task root (default: <script>/tasks)

NOTE: base HumanEval is heavily contaminated (models memorized it) and its
tests are weak — fine for proving the harness mechanism + an A/B *runs*, but for
a trustworthy memory signal graduate to a less-contaminated, agentic set (Aider
polyglot / SWE-rebench). The dir layout is identical; only this importer swaps.
"""
import argparse
import gzip
import json
import os
import stat
import urllib.request

DEFAULT_URL = "https://raw.githubusercontent.com/openai/human-eval/master/data/HumanEval.jsonl.gz"

PROMPT = (
    "Complete the function in solution.py so it satisfies its docstring. Edit "
    "solution.py directly — keep the existing signature, implement the body, and "
    "don't add example usage, prints, or a __main__ block."
)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--limit", type=int, default=0, help="first N problems (0 = all)")
    ap.add_argument(
        "--out",
        default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "tasks"),
    )
    ap.add_argument("--url", default=DEFAULT_URL)
    args = ap.parse_args()

    with urllib.request.urlopen(args.url) as resp:  # noqa: S310 (trusted bench URL)
        raw = gzip.decompress(resp.read()).decode()
    problems = [json.loads(line) for line in raw.splitlines() if line.strip()]
    if args.limit:
        problems = problems[: args.limit]

    for p in problems:
        tid = "he_" + p["task_id"].replace("/", "_")
        d = os.path.join(args.out, tid)
        os.makedirs(os.path.join(d, "workspace"), exist_ok=True)
        os.makedirs(os.path.join(d, "grader"), exist_ok=True)

        # The agent's starting file: signature + docstring, no body.
        _write(os.path.join(d, "workspace", "solution.py"), p["prompt"])
        _write(os.path.join(d, "prompt.txt"), PROMPT + "\n")

        # The hidden test: import the agent's entry point as `candidate`, run the
        # problem's own `check(candidate)`.
        check = (
            "import sys\n"
            "sys.path.insert(0, '.')\n"
            f"from solution import {p['entry_point']} as candidate\n\n"
            f"{p['test']}\n"
            "check(candidate)\n"
            "print('PASS')\n"
        )
        _write(os.path.join(d, "grader", "check.py"), check)
        _write(os.path.join(d, "grader", "grade.sh"), "#!/bin/sh\npython3 check.py\n")

    print(f"imported {len(problems)} HumanEval task(s) → {args.out}")


def _write(path: str, content: str) -> None:
    with open(path, "w") as f:
        f.write(content)
    if path.endswith(".sh"):
        os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC)


if __name__ == "__main__":
    main()
