#!/usr/bin/env python3
"""Generate the memory-validity task family — one task per kypp validity lever.

The whole point: each task's correct answer is OUT-OF-BAND — it lives ONLY in
memory, never in the workspace — so a memory-OFF run genuinely cannot solve it and
any memory-ON success is attributable to recall, not competence. That's what makes
this measurable where the optimization gate wasn't: a planted answer + a near-binary
grader, like the sensitivity check that passed.

Each lever isolates ONE kypp mechanism:
  recency       — a stale fact, then a correction. The brief must surface the NEW
                  value and NOT resurface the stale one (supersession).
  authority     — an agent guess (candidate) AND a human fact coexist. Recall must
                  prefer the human value over the lower-rank guess.
  corroboration — two independent sessions agree (promoted accepted); one dissents
                  (stays candidate). The corroborated value must win.
  scope         — a fact lives in a DIFFERENT project. It must NOT leak into this
                  project's brief (a false-application test).
  pitfall       — a negative lesson ("approach X fails, use Y"). The agent must
                  avoid X.

Output: scripts/eval/memory/tasks/<lever>/ with prompt.txt, workspace/<file>,
grader/grade.sh, expected.json (lever + project + seed ops + grade tokens + metric).
The matrix harness consumes these; the cost-router reuses _seed_runner for recording.

  python3 gen-memory-tasks.py [--out DIR]      # write the family
  python3 gen-memory-tasks.py --self-test      # validate WITHOUT an agent (the gate
                                               # before the gate): out-of-band integrity
                                               # + the seed produces the intended brief
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
PROJECT = "memtest"
OTHER_PROJECT = "memtest-other"  # for the scope leak test

# Each lever: the workspace file (with a placeholder, NO hint of the answer), the
# task prompt, the seed ops (what memory knows), the grade tokens, and the metric.
#   metric "lift"      → memory-ON should apply `present`/avoid `absent`; OFF can't.
#   metric "false_app" → memory must NOT cause the wrong value to appear (scope leak).
LEVERS = [
    {
        "lever": "recency",
        "file": "config.py",
        "workspace": "# deploy config\nDEPLOY_TARGET = \"TODO\"\n",
        "prompt": (
            "Edit config.py: set DEPLOY_TARGET to the CURRENT production deploy target "
            "host. Use the value from project memory if present. Edit the file."
        ),
        "present": ["west-2.deploy.internal"],
        "absent": ["east-1.deploy.internal"],
        "metric": "lift",
        "seed": [
            {"op": "claim", "type": "fact", "subject": "deploy target",
             "content": "the production deploy target host is east-1.deploy.internal",
             "accept": True},
            {"op": "correct", "subject": "deploy target",
             "content": "the production deploy target host is west-2.deploy.internal"},
        ],
    },
    {
        "lever": "authority",
        "file": "settings.py",
        "workspace": "# service settings\nAPI_REGION = \"TODO\"\n",
        "prompt": (
            "Edit settings.py: set API_REGION to the correct API region from project "
            "memory. Edit the file."
        ),
        "present": ["us-east-2"],
        "absent": ["eu-central-1"],
        "metric": "lift",
        "seed": [
            # An agent guess: high confidence but still a candidate (not accepted).
            {"op": "claim", "type": "fact", "subject": "api region",
             "content": "the api region is eu-central-1", "confidence": 0.9},
            # A human fact: lower nominal confidence, but human authority outranks it.
            {"op": "claim", "type": "fact", "subject": "api region",
             "content": "the api region is us-east-2", "authority": "human"},
            # NOTE: deliberately no consolidate — both coexist; recall must PREFER the
            # human one. (consolidate would supersede, which is the recency lever.)
        ],
    },
    {
        "lever": "corroboration",
        "file": "pool.py",
        "workspace": "# connection pool\nCONNECTION_PROFILE = \"TODO\"\n",
        "prompt": (
            "Edit pool.py: set CONNECTION_PROFILE to the agreed connection profile from "
            "project memory. Edit the file."
        ),
        "present": ["high-throughput"],
        "absent": ["conservative"],
        "metric": "lift",
        "seed": [
            # Two INDEPENDENT sessions agree on the SAME subject → corroborated →
            # consolidate promotes the survivor to accepted → it surfaces in the brief.
            {"op": "claim", "type": "fact", "subject": "connection profile",
             "content": "the agreed connection profile is high-throughput",
             "source_ids": ["sess-a"]},
            {"op": "claim", "type": "fact", "subject": "connection profile",
             "content": "the agreed connection profile is high-throughput",
             "source_ids": ["sess-b"]},
            {"op": "consolidate", "subject": "connection profile", "accept_corroboration": 2},
            # A single-source, uncorroborated proposal under a DIFFERENT subject → stays
            # a candidate → the default (accepted-only) brief must NOT surface it.
            # (Same-subject would just dedup-conflict; corroboration ≠ conflict-resolution.)
            {"op": "claim", "type": "fact", "subject": "connection profile (unverified proposal)",
             "content": "an unverified proposal is a conservative connection profile",
             "source_ids": ["sess-c"]},
        ],
    },
    {
        "lever": "scope",
        "file": "secrets.py",
        "workspace": "# secret manager\n# Set SECRET_PATH to this project's vault path,\n# or leave as TODO if unknown.\nSECRET_PATH = \"TODO\"\n",
        "prompt": (
            "Edit secrets.py: set SECRET_PATH to THIS project's secret manager path. "
            "If you do not know it for this project, leave it as \"TODO\". Edit the file."
        ),
        "present": [],
        "absent": ["/vault/secrets/other-proj"],  # belongs to a DIFFERENT project
        "metric": "false_app",
        "seed": [
            # Seeded into ANOTHER project — must never appear in THIS project's brief.
            {"op": "claim", "type": "fact", "subject": "secret manager path",
             "content": "the secret manager path is /vault/secrets/other-proj",
             "accept": True, "project": OTHER_PROJECT},
        ],
    },
    {
        "lever": "pitfall",
        "file": "fetch.py",
        "workspace": "# TODO: implement get_json(url): fetch the URL and return parsed JSON.\n# Pick an HTTP client library and use it.\n",
        "prompt": (
            "Edit fetch.py: implement get_json(url) that fetches the URL and returns "
            "parsed JSON. Heed any project-memory guidance about which HTTP client to "
            "use. Edit the file."
        ),
        "present": ["httpx"],
        "absent": ["import requests"],
        "metric": "lift",
        "seed": [
            {"op": "claim", "type": "pitfall", "subject": "http client choice",
             "content": ("Using the `requests` library in this codebase causes connection-pool "
                         "exhaustion under load. Use `httpx` instead for all HTTP calls."),
             "accept": True},
        ],
    },
]


def grade_script(present: list[str], absent: list[str]) -> str:
    """A tiny, near-binary host grader: every `present` token must appear in the edited
    workspace and no `absent` token may. Excludes grade.sh itself (it names the tokens,
    which would self-match under grep -r)."""
    lines = ["#!/bin/sh", "# Auto-generated. Exit 0 = memory correctly applied.", "miss=0"]
    for v in present:
        lines.append(f"grep -rqF -- '{v}' . --exclude=grade.sh || {{ echo 'missing: {v}'; miss=1; }}")
    for v in absent:
        lines.append(f"if grep -rqF -- '{v}' . --exclude=grade.sh; then echo 'leaked (should be absent): {v}'; miss=1; fi")
    lines += ['[ "$miss" = 0 ] && { echo ok; exit 0; }', "exit 1", ""]
    return "\n".join(lines)


def write_family(out: str):
    for spec in LEVERS:
        d = os.path.join(out, spec["lever"])
        ws = os.path.join(d, "workspace")
        gr = os.path.join(d, "grader")
        os.makedirs(ws, exist_ok=True)
        os.makedirs(gr, exist_ok=True)
        with open(os.path.join(ws, spec["file"]), "w") as f:
            f.write(spec["workspace"])
        with open(os.path.join(d, "prompt.txt"), "w") as f:
            f.write(spec["prompt"] + "\n")
        gs = os.path.join(gr, "grade.sh")
        with open(gs, "w") as f:
            f.write(grade_script(spec["present"], spec["absent"]))
        os.chmod(gs, 0o755)
        meta = {k: spec[k] for k in ("lever", "file", "present", "absent", "metric", "seed")}
        meta["project"] = OTHER_PROJECT if spec["lever"] == "scope" else PROJECT
        meta["brief_project"] = PROJECT  # the project the agent's brief is drawn from
        with open(os.path.join(d, "expected.json"), "w") as f:
            json.dump(meta, f, indent=2)
        print(f"  wrote {spec['lever']:14s} → {d}")
    print(f"\n{len(LEVERS)} memory-lever tasks → {out}")


def _kypp_python() -> str:
    """The interpreter the `kypp` console script runs under (so `import kypp` works)."""
    kypp = shutil.which("kypp")
    if not kypp:
        raise SystemExit("self-test: `kypp` not on PATH")
    with open(kypp) as f:
        shebang = f.readline().strip()
    return shebang[2:].strip() if shebang.startswith("#!") else "python3"


def _seed(py: str, db: str, brief_project: str, ops: list) -> None:
    env = {**os.environ, "KYPP_MEMORY_DB": db, "KYPP_PROJECT": brief_project}
    subprocess.run([py, os.path.join(HERE, "_seed_runner.py"), brief_project],
                   input=json.dumps(ops), text=True, env=env, check=True,
                   capture_output=True)


def _briefing(db: str, project: str) -> str:
    env = {**os.environ, "KYPP_MEMORY_DB": db, "KYPP_PROJECT": project}
    p = subprocess.run(["kypp", "briefing", "--project", project],
                       env=env, capture_output=True, text=True)
    return p.stdout


def self_test() -> int:
    """No agent: prove the experiment is sound. For each lever — (1) the answer is NOT
    in the workspace (out-of-band integrity), (2) after seeding, the brief surfaces the
    intended value and NOT the wrong one (the lever actually works at the kypp level).
    If this fails, no agent run is worth doing."""
    py = _kypp_python()
    ok = True
    for spec in LEVERS:
        lever = spec["lever"]
        # (1) out-of-band integrity: no present/absent token already in the workspace.
        blob = spec["workspace"]
        for v in spec["present"] + spec["absent"]:
            if v in blob:
                print(f"  ✗ {lever}: token {v!r} is IN the workspace — not out-of-band")
                ok = False

        with tempfile.TemporaryDirectory() as tmp:
            db = os.path.join(tmp, "k.db")
            _seed(py, db, PROJECT, spec["seed"])
            brief = _briefing(db, PROJECT)

            if spec["metric"] == "lift":
                for v in spec["present"]:
                    if v not in brief:
                        print(f"  ✗ {lever}: brief MISSING expected {v!r} — lever broken at kypp level")
                        print(f"      brief was: {brief.strip()[:200]!r}")
                        ok = False
                for v in spec["absent"]:
                    if v in brief:
                        print(f"  ✗ {lever}: brief LEAKED stale/wrong {v!r} — lever broken")
                        ok = False
            elif spec["metric"] == "false_app":
                # The wrong value lives in OTHER_PROJECT; THIS project's brief must omit it.
                for v in spec["absent"]:
                    if v in brief:
                        print(f"  ✗ {lever}: cross-project leak — {v!r} in {PROJECT}'s brief")
                        ok = False
            if ok:
                print(f"  ✓ {lever}: out-of-band + brief correct")
    print("\nself-test: " + ("PASS — family is sound, agent runs are warranted" if ok
                              else "FAIL — fix the family before spending agent runs"))
    return 0 if ok else 1


def main():
    ap = argparse.ArgumentParser(description="generate the memory-validity task family")
    ap.add_argument("--out", default=os.path.join(HERE, "tasks"))
    ap.add_argument("--self-test", action="store_true",
                    help="validate out-of-band integrity + brief correctness, no agent")
    args = ap.parse_args()
    if args.self_test:
        raise SystemExit(self_test())
    write_family(args.out)


if __name__ == "__main__":
    main()
