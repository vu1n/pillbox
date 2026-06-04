#!/usr/bin/env python3
"""wire.py — bridge a session's §0 event stream into the memory store (the capture side of the loop).

Reads real §0 events — from `pillbox session log ID` piped on stdin, a `log.jsonl` path, or `--session
ID` (resolved under ~/.pillbox) — and records the verifiable OUTCOME signals (the grade, a run
failure, the tool-failure tally) as OBSERVATIONS. That closes the loop from "an agent worked" to "the
store has the raw record"; `--distill` also runs distill.py (events → claims) in the same pass.

The §0 event contract is src/contract.rs (camelCase envelope, payload tagged on `type`); we reuse
distill.build_trace to parse it, so the two stay in lockstep. Observing is idempotent per log file via
a `.ghost-observed` marker (re-running a finished session's log is a no-op), matching `session ingest`.

Source is decoupled from sink: any list of §0 event dicts feeds observe_events — a file, stdin JSONL,
or a future `session subscribe` WebSocket adapter.
"""
from __future__ import annotations

import argparse
import glob
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from distill import build_trace, distill_session, distiller_from_env, read_log  # noqa: E402 — sibling
from store import MemoryStore, store_from_env  # noqa: E402

_FB = 400  # outcome-feedback cap in an observation (the raw record, not the full grader dump)


def observe_events(events: list[dict], store, *, project: str, scope: str = "project",
                   actor: str = "session") -> list[str]:
    """Record a session's verifiable OUTCOME signals as observations: the grade (`scored`), a run
    failure, and the tool-failure tally. Bounded (≤3 per session) and high-signal — the raw record
    distill later draws on; not one-observation-per-event (that's the trajectory, which distill reads
    from the events directly). Returns the new observation ids."""
    t = build_trace(events)
    src = f"session:{t.session_id}" if t.session_id else None
    oids: list[str] = []

    if t.verdict:
        v = t.verdict
        failed = [c.get("name", "?") for c in v.failed_criteria]
        content = f"graded {'PASS' if v.passed else 'FAIL'} score={v.score} ({v.grader})"
        if failed:
            content += f"; failed: {', '.join(failed[:5])}" + (" …" if len(failed) > 5 else "")
        if v.feedback:
            content += f" — {v.feedback[:_FB]}"
        oids.append(store.observe(actor, content, scope=scope, project=project, source=src,
                                  confidence=0.9, metadata={"kind": "scored", "passed": v.passed,
                                                            "score": v.score}))
    if t.run_failed:
        oids.append(store.observe(actor, f"run failed: {t.run_failed}", scope=scope, project=project,
                                  source=src, confidence=0.8, metadata={"kind": "run_failed"}))
    fails = t.tool_failures
    if fails:
        tally = ", ".join(f"{n}×{name}" for name, n in fails.most_common())
        oids.append(store.observe(actor, f"{sum(fails.values())} tool failures: {tally}", scope=scope,
                                  project=project, source=src, confidence=0.6,
                                  metadata={"kind": "tool_failures", "by_tool": dict(fails)}))
    return oids


def resolve_session_log(sid: str) -> str | None:
    """Find a session's log.jsonl under ~/.pillbox by id (accepts a prefix). Sessions live at
    <pillbox-state>/sessions/<id>/log.jsonl across global + project pillboxes."""
    for pat in (f"~/.pillbox/*/sessions/{sid}*/log.jsonl", f"~/.pillbox/projects/*/sessions/{sid}*/log.jsonl"):
        hits = sorted(glob.glob(os.path.expanduser(pat)))
        if hits:
            return hits[0]
    return None


def main():
    ap = argparse.ArgumentParser(description="bridge a session's §0 log into the memory store")
    ap.add_argument("source", nargs="?", help="path to a §0 log.jsonl, or - for stdin (e.g. `pillbox session log ID | wire.py -`)")
    ap.add_argument("--session", help="resolve a session id (or prefix) to its log under ~/.pillbox")
    ap.add_argument("--project", default=os.environ.get("GHOST_PROJECT", "default"))
    ap.add_argument("--task", default="", help="the session's prompt (context for --distill)")
    ap.add_argument("--distill", action="store_true", help="also distill claims (heuristic) in the same pass")
    args = ap.parse_args()

    logpath = None
    if args.source == "-":
        events = [json.loads(line) for line in sys.stdin if line.strip()]
    else:
        logpath = args.source or (resolve_session_log(args.session) if args.session else None)
        if not logpath:
            ap.error("need a log path, - for stdin, or --session ID")
        if os.path.exists(logpath + ".ghost-observed"):
            print(f"already observed: {logpath}")
            return
        events = read_log(logpath)

    store = store_from_env()
    oids = observe_events(events, store, project=args.project)
    # --distill picks the configured distiller (GHOST_DISTILL_MODEL → LLM+heuristic fallback, else heuristic)
    cids = (distill_session(events, store, project=args.project, task=args.task,
                            distiller=distiller_from_env()) if args.distill else [])
    if logpath:
        open(logpath + ".ghost-observed", "w").close()  # idempotency marker (matches `session ingest`)

    msg = f"observed {len(oids)} outcome signal(s)"
    if args.distill:
        msg += f", distilled {len(cids)} claim(s)"
    print(f"{msg} from {len(events)} §0 events → project {args.project!r}")


if __name__ == "__main__" and len(sys.argv) > 1:
    main()
elif __name__ == "__main__":
    # self-test: observe_events over a synthetic §0 trace (no real session needed).
    import shutil

    db = "/tmp/wire-selftest.db"
    for f in glob.glob(db + "*"):
        try: os.remove(f)
        except OSError: pass

    events = [
        {"sessionId": "sess9", "payload": {"type": "message_end", "messageId": "m", "model": "x"}},
        {"sessionId": "sess9", "payload": {"type": "tool_call", "name": "Bash", "status": "error",
                                           "output": "boom", "input": {"command": "cargo build"}}},
        {"sessionId": "sess9", "payload": {"type": "tool_call", "name": "Bash", "status": "error", "output": "boom2"}},
        {"sessionId": "sess9", "payload": {"type": "tool_call", "name": "Read", "status": "completed", "output": "ok"}},
        {"sessionId": "sess9", "payload": {"type": "scored", "grader": "rubric", "passed": False, "score": 0.25,
                                           "feedback": "3/4 failed", "criteria": [{"name": "builds", "passed": False}]}},
        {"sessionId": "sess9", "payload": {"type": "run_failed", "reason": "agent exited 1", "exitCode": 1}},
    ]

    store = MemoryStore(db)
    oids = observe_events(events, store, project="pillbox")
    assert len(oids) == 3, oids  # scored + run_failed + tool_failures (the single completed tool excluded)

    # observations are the raw layer (recall reads claims); verify via the table directly.
    cur = store.db.cursor()
    rows = cur.execute("SELECT content, metadata FROM observations WHERE source = ? ORDER BY content",
                       ("session:sess9",)).fetchall()
    kinds = {json.loads(m)["kind"] for _, m in rows}
    assert kinds == {"scored", "run_failed", "tool_failures"}, kinds
    scored = next(c for c, _ in rows if c.startswith("graded"))
    assert "FAIL" in scored and "builds" in scored, scored
    tally = next(c for c, _ in rows if "tool failures" in c)
    assert "2×Bash" in tally, tally  # the Read (completed) is not counted

    # --distill path lands claims in the same store
    cids = distill_session(events, store, project="pillbox", task="build it")
    assert cids and store.recall("run failed build", project="pillbox", include_candidates=True)

    print(f"OK — wire §0→observe: recorded {len(oids)} outcome observations "
          f"({sorted(kinds)}) from a {len(events)}-event trace; --distill adds {len(cids)} claim(s)")
