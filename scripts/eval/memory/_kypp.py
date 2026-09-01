"""Shared kypp-invocation helpers for the memory-dir consumers (gen-memory-tasks.py,
memory-matrix.py) — the one place that knows HOW to call kypp.

Scope note: only the two scripts in THIS directory import this (same-dir import; these
run as standalone scripts, not a package). cost-router.py lives in scripts/router/ and
keeps its own copy on purpose — a cross-directory `import _kypp` would need a sys.path
hack, whereas the shared `_seed_runner.py` crosses dirs cleanly because it's spawned as
a subprocess PATH, not imported.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess

HERE = os.path.dirname(os.path.abspath(__file__))
SEED_RUNNER = os.path.join(HERE, "_seed_runner.py")


def kypp_python() -> str:
    """The interpreter the `kypp` console script runs under, so `_seed_runner.py`'s
    `import kypp` resolves (kypp may be installed in an isolated env, not on the
    ambient python3)."""
    kypp = shutil.which("kypp")
    if not kypp:
        raise SystemExit("`kypp` not on PATH")
    with open(kypp) as f:
        sb = f.readline().strip()
    return sb[2:].strip() if sb.startswith("#!") else "python3"


def seed(py: str, db: str, project: str, ops: list) -> None:
    env = {**os.environ, "KYPP_MEMORY_DB": db, "KYPP_PROJECT": project}
    p = subprocess.run([py, SEED_RUNNER, project], input=json.dumps(ops), text=True,
                       env=env, capture_output=True)
    if p.returncode != 0:
        # Surface the child's diagnostic — a swallowed seed failure would silently
        # corrupt an arm (e.g. memory-on running as memory-off).
        raise RuntimeError(f"_seed_runner failed (project={project}): {p.stderr.strip()}")


def briefing(db: str, project: str) -> str:
    env = {**os.environ, "KYPP_MEMORY_DB": db, "KYPP_PROJECT": project}
    p = subprocess.run(["kypp", "briefing", "--project", project],
                       env=env, capture_output=True, text=True)
    return p.stdout.strip()
