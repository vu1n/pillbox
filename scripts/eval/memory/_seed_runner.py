#!/usr/bin/env python3
"""Seed a kypp store from a JSON op-list on stdin — the one place that touches the
kypp.store API, so both the memory matrix and the cost-router seed identically.

MUST run under the SAME interpreter the `kypp` console script uses (so `import kypp`
resolves) — callers find it via `kypp_python()` (the shebang of `which kypp`), not
the ambient python3. Reads KYPP_MEMORY_DB / KYPP_PROJECT from env (store_from_env).

Ops (one JSON object each, list on stdin):
  {"op":"claim","type":"fact","subject":S,"content":C, ...}   # authority/source_ids/accept/project/confidence/code_refs/verify
  {"op":"correct","subject":S,"content":C, ...}               # human authority + consolidate(subject) → supersedes prior
  {"op":"consolidate","subject":S?,"accept_corroboration":K}  # promote corroborated candidates / dedup

Why an op-list, not generated code: the seeds are DATA (checked into expected.json),
so a task family is inspectable and diffable, and nothing exec's generated python.
"""
from __future__ import annotations

import json
import os
import sys

from kypp.store import store_from_env
from kypp.arbiter import consolidate


def main() -> int:
    project = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("KYPP_PROJECT", "memtest")
    ops = json.load(sys.stdin)
    s = store_from_env()
    n = 0
    for op in ops:
        k = op["op"]
        proj = op.get("project", project)
        if k == "claim":
            s.claim(
                op.get("type", "fact"), op["subject"], op["content"],
                scope=op.get("scope", "project"), project=proj,
                confidence=op.get("confidence", 0.7), source_ids=op.get("source_ids", []),
                accept=op.get("accept", False), authority=op.get("authority", "agent"),
                code_refs=op.get("code_refs", []), verify=op.get("verify"),
            )
        elif k == "correct":
            # Human correction → auto-accepts, then consolidate buries the prior claim
            # on the same subject (the supersession lever).
            s.claim(
                op.get("type", "fact"), op["subject"], op["content"],
                scope=op.get("scope", "project"), project=proj,
                confidence=op.get("confidence", 0.95), authority="human",
            )
            consolidate(s, project=proj, subject=op["subject"])
        elif k == "consolidate":
            consolidate(
                s, project=proj, subject=op.get("subject"),
                accept_corroboration=op.get("accept_corroboration", 2),
                semantic=op.get("semantic"),
            )
        else:
            raise SystemExit(f"_seed_runner: unknown op {k!r}")
        n += 1
    print(f"_seed_runner: applied {n} op(s) into project={project}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
