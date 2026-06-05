#!/usr/bin/env python3
"""autocapture.py — fill swarm memory from completed sessions, automatically (the loop, closed).

An idempotent SWEEP over §0 session logs: for each COMPLETED, not-yet-captured session, run the wire
capture (observe outcomes + distill claims). No daemon, no webhook — run it on a schedule (cron) or
after a batch of runs, matching pillbox's cron-the-maintenance model (like `session prune`). Re-running
is a no-op (wire's .ghost-observed marker), and in-flight sessions are skipped until they finish, so
it's safe to run as often as you like.

"Completed" = the §0 log has a terminal event (scored / run_finished / run_failed). The capture core
is shared with wire.py (capture_log_file), so the eventual pillbox-native path — a `pillbox run
--memory` flag invoking the same capture on session.done — is the event-driven sibling of this sweep.
"""
from __future__ import annotations

import argparse
import glob
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from distill import distiller_from_env  # noqa: E402 — siblings, path set above
from store import store_from_env  # noqa: E402
from wire import capture_log_file  # noqa: E402

DEFAULT_LOGS = "~/.pillbox/*/sessions/*/log.jsonl"


def sweep(store, *, project: str, log_glob: str = DEFAULT_LOGS, distiller=None) -> dict:
    """Capture every completed, uncaptured session under `log_glob`. Returns {captured, skipped,
    observations, claims} — skipped counts in-flight + already-captured logs."""
    captured = skipped = obs = claims = 0
    for path in glob.glob(os.path.expanduser(log_glob)):
        res = capture_log_file(path, store, project=project, distiller=distiller, require_complete=True)
        if res is None:
            skipped += 1
            continue
        captured += 1
        obs += res["observations"]
        claims += res["claims"]
    return {"captured": captured, "skipped": skipped, "observations": obs, "claims": claims}


def main():
    ap = argparse.ArgumentParser(description="sweep completed sessions into swarm memory (idempotent)")
    ap.add_argument("--project", default=os.environ.get("GHOST_PROJECT", "default"))
    ap.add_argument("--logs", default=DEFAULT_LOGS, help="glob of §0 log.jsonl files")
    ap.add_argument("--no-distill", action="store_true", help="record outcome observations only, skip claims")
    args = ap.parse_args()

    distiller = None if args.no_distill else distiller_from_env()
    res = sweep(store_from_env(), project=args.project, log_glob=args.logs, distiller=distiller)
    print(f"autocapture: captured {res['captured']} new session(s), skipped {res['skipped']} "
          f"(in-flight or already captured) → {res['observations']} observations, {res['claims']} claims, "
          f"project {args.project!r}")


if __name__ == "__main__" and len(sys.argv) > 1:
    main()
elif __name__ == "__main__":
    # self-test: a temp sessions tree — one completed (has `scored`), one in-flight (no terminal event).
    import json
    import shutil

    from distill import HeuristicDistiller
    from store import MemoryStore

    root = "/tmp/autocap-selftest"
    shutil.rmtree(root, ignore_errors=True)
    done, live = os.path.join(root, "sessions", "done"), os.path.join(root, "sessions", "live")
    os.makedirs(done); os.makedirs(live)

    def write_log(d, events):
        with open(os.path.join(d, "log.jsonl"), "w") as f:
            for e in events:
                f.write(json.dumps(e) + "\n")

    write_log(done, [
        {"sessionId": "done", "payload": {"type": "tool_call", "name": "Bash", "status": "error", "output": "x"}},
        {"sessionId": "done", "payload": {"type": "tool_call", "name": "Bash", "status": "error", "output": "y"}},
        {"sessionId": "done", "payload": {"type": "scored", "grader": "r", "passed": False, "score": 0.0, "criteria": []}},
    ])
    write_log(live, [{"sessionId": "live", "payload": {"type": "tool_call", "name": "Read", "status": "completed", "output": "z"}}])

    db = "/tmp/autocap-selftest.db"
    for f in glob.glob(db + "*"):
        try: os.remove(f)
        except OSError: pass
    store = MemoryStore(db)
    glob_pat = os.path.join(root, "sessions", "*", "log.jsonl")

    res = sweep(store, project="p", log_glob=glob_pat, distiller=HeuristicDistiller())
    assert res["captured"] == 1 and res["skipped"] == 1, res  # done captured; live skipped (in-flight)
    assert res["claims"] >= 1, res
    assert os.path.exists(os.path.join(done, "log.jsonl.ghost-observed")), "completed → marked"
    assert not os.path.exists(os.path.join(live, "log.jsonl.ghost-observed")), "in-flight → unmarked (retry later)"
    assert sweep(store, project="p", log_glob=glob_pat, distiller=HeuristicDistiller())["captured"] == 0, "idempotent"

    shutil.rmtree(root, ignore_errors=True)
    for f in glob.glob(db + "*"):
        try: os.remove(f)
        except OSError: pass
    print("OK — autocapture sweep: captured the completed session, skipped the in-flight one, idempotent on re-run")
