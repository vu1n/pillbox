#!/usr/bin/env python3
"""batch_distill.py — re-distill a corpus of §0 logs with the LLM distiller, work-deduped by task.

The heuristic seed gives task-specific claims; the LLM generalizes (durable lessons). Re-distilling
every repeat of the same task wastes LLM compute and yields dup claims, so DEDUP THE WORK: group
sessions by task signature (the rubric's criteria-name set — task-specific names like BookStoreTest…),
LLM-distill ONE representative per task (the run closest to a partial score — most instructive,
shows the pass/fail boundary), then consolidate. Backend = distiller_from_env (GHOST_DISTILL_MODEL);
without the model set this degrades to a heuristic re-distill, which is pointless — it warns.
"""
from __future__ import annotations

import argparse
import glob
import hashlib
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from arbiter import consolidate  # noqa: E402 — siblings, path set above
from distill import build_trace, distill_session, distiller_from_env, read_log  # noqa: E402
from store import store_from_env  # noqa: E402


def task_signature(trace) -> str:
    """Task identity = its rubric's criteria-name set (task-specific). No rubric → its own group."""
    if trace.verdict and trace.verdict.criteria:
        names = "\n".join(sorted(c.get("name", "") for c in trace.verdict.criteria))
        return "rubric:" + hashlib.sha1(names.encode()).hexdigest()[:12]
    return "solo:" + trace.session_id


def plan_corpus(log_glob: str) -> tuple[list[tuple[str, str, float | None]], int]:
    """Pass 1 (cheap — parse, keep only sig/score/path, discard events): group sessions by task and
    pick one representative each (closest to a partial score). Returns (reps, total_session_count),
    reps = [(sig, path, score)] most-learnable first."""
    groups: dict[str, list[tuple[float | None, str]]] = {}
    for lp in glob.glob(log_glob):
        try:
            t = build_trace(read_log(lp))
        except Exception:
            continue
        if not t.event_count:
            continue
        score = t.verdict.score if t.verdict else (0.0 if t.run_failed else None)
        groups.setdefault(task_signature(t), []).append((score, lp))

    reps = []
    for sig, members in groups.items():
        score, path = min(members, key=lambda m: abs((m[0] or 0.0) - 0.5))  # closest to partial
        reps.append((sig, path, score))
    reps.sort(key=lambda r: (r[2] is None, abs((r[2] or 0.0) - 0.5)))  # most-learnable first
    return reps, sum(len(m) for m in groups.values())


def main():
    ap = argparse.ArgumentParser(description="LLM re-distill of a §0 corpus, one rep per task")
    ap.add_argument("--logs", default=os.path.expanduser("~/.pillbox/*/sessions/*/log.jsonl"))
    ap.add_argument("--project", default=os.environ.get("GHOST_PROJECT", "eval-polyglot"))
    ap.add_argument("--limit", type=int, default=0, help="cap representatives distilled (0 = all)")
    ap.add_argument("--plan", action="store_true", help="report the task grouping; distill nothing")
    args = ap.parse_args()

    reps, n_sessions = plan_corpus(args.logs)
    n_tasks = len(reps)
    if args.limit:
        reps = reps[:args.limit]
    print(f"{n_sessions} sessions → {n_tasks} distinct tasks; distilling {len(reps)} representative(s)",
          flush=True)
    if args.plan:
        for sig, path, score in reps[:40]:
            print(f"  {sig}  score={score}  {os.path.basename(os.path.dirname(path))}")
        return
    if not os.environ.get("GHOST_DISTILL_MODEL"):
        print("WARNING: GHOST_DISTILL_MODEL unset — re-distilling with the heuristic (no LLM gain)",
              file=sys.stderr)

    store, distiller = store_from_env(), distiller_from_env()
    n_claims, t0 = 0, time.monotonic()
    for i, (sig, path, score) in enumerate(reps, 1):
        try:
            cids = distill_session(read_log(path), store, project=args.project,
                                   task=f"(eval task {sig})", distiller=distiller)
            n_claims += len(cids)
            print(f"  [{i}/{len(reps)}] {len(cids)} claims  score={score}  "
                  f"{os.path.basename(os.path.dirname(path))}", flush=True)
        except Exception as e:
            print(f"  [{i}/{len(reps)}] ERR {type(e).__name__}: {str(e)[:80]}", flush=True)
    res = consolidate(store, project=args.project)
    print(f"\ndistilled {n_claims} claims from {len(reps)} tasks in {time.monotonic() - t0:.0f}s; "
          f"consolidate superseded {res['superseded']} across {res['groups']} dup group(s)", flush=True)


if __name__ == "__main__" and len(sys.argv) > 1:
    main()
elif __name__ == "__main__":
    # self-test: task grouping (same criteria names → same signature; different → different).
    def _trace(names):
        return build_trace([{"sessionId": "s", "payload": {"type": "scored", "grader": "r",
                             "passed": False, "score": 0.5,
                             "criteria": [{"name": n, "passed": False} for n in names]}}])
    assert task_signature(_trace(["a", "b"])) == task_signature(_trace(["b", "a"])), "order-independent"
    assert task_signature(_trace(["a", "b"])) != task_signature(_trace(["a", "c"])), "distinct tasks differ"
    assert task_signature(build_trace([{"sessionId": "x", "payload": {"type": "message_end", "messageId": "m"}}])).startswith("solo:")
    print("OK — batch_distill: task signature groups by criteria-name set (order-independent, distinct tasks separate)")
