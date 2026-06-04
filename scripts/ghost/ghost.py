#!/usr/bin/env python3
"""ghost — the meta-harness (router). v1.

"Ghost in the Shell": the ghost is the orchestrating intelligence; the shells are
the pillbox containers running interchangeable cheap/local models. The ghost
decides WHICH shell (model) handles a task; the shells do the work. v1 is the
ROUTER (pick a model per task); decomposition (break a task into sub-tasks, fan
out) is v2 on top of this.

v1 is deliberately NOT yet a learned/DSPy router — it establishes the cost↔quality
picture with simple policies so we can MEASURE before optimizing (the hard-won
lesson from the gate). A DSPy policy slots in later as just another `route()` arm.

Policies:
  always:<model>        — always this model (the per-tier baselines).
  cascade:<m1,m2,...>   — try cheapest first; escalate to the next iff the rubric
                          score is below THRESHOLD. Cost = sum of attempts.
  (future) dspy         — a learned predictor: task → model, optimized on the
                          cost-adjusted reward collected from real usage.

Metric = COST-ADJUSTED QUALITY: mean rubric score AND total cost, so we can see
whether a router matches frontier quality at a fraction of the cost (the v1 thesis).
Reuses gate.py's substrate (run/score/frozen tasks). Determinism: run workers at
temperature 0 (set in the opencode provider config) so a few trials suffice.

Usage:
  python3 ghost/ghost.py --policy cascade:ollama/qwen3.6:35b-a3b-coding-nvfp4,\\
      zai-coding-plan/glm-4.5-air,zai-coding-plan/glm-5.1 --task-set aider --limit 4
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import tempfile
from dataclasses import dataclass
from statistics import mean

# Reuse the proven pillbox substrate (the run→score→tasks loop) from the gate rig.
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "eval"))
from gate import Config as SubstrateConfig  # noqa: E402
from gate import Pillbox, _task_dir, bookmarks, graded_run  # noqa: E402

# Relative cost per task-run by model — the knob the router optimizes against.
# Local ≈ compute-only; hosted tiers are illustrative $ ratios (tune to reality).
COST = {
    "ollama/qwen3.6:35b-a3b-coding-nvfp4": 0.1,  # local: electricity + time, no API $
    "zai-coding-plan/glm-4.5-air": 1.0,          # cheap hosted
    "zai-coding-plan/glm-5.1": 4.0,              # frontier-tier (most capable we have)
}
DEFAULT_COST = 1.0  # untabled model → placeholder cost (its run still scores/fails normally)
# cascade escalates while score < THRESHOLD (i.e., not all criteria pass). CAVEAT (measured,
# ghost v1): a strict threshold + a cheap tier that never clears it makes cascade cost-DOMINATED
# — every task pays the cheap attempt AND escalates anyway (cost 5 > glm-5.1's 4 at equal
# quality). Cheap-first only pays when the cheap tier actually passes a real fraction of tasks;
# otherwise the lever is PREDICTIVE routing (pick the tier upfront), not try-then-escalate.
THRESHOLD = 1.0


def cost_of(model: str) -> float:
    return COST.get(model, DEFAULT_COST)


def run_policy(pb: Pillbox, policy: str, task_dir: str) -> dict:
    """Route + run one task under `policy`; return {score, cost, models, error}.
    Score = best rubric score across attempts; cost = sum of every attempt's model cost."""
    kind, _, arg = policy.partition(":")
    if kind == "always":
        r = graded_run(pb, task_dir, "", arg)
        return {"score": r["score"], "cost": cost_of(arg), "models": [arg], "error": r["error"]}
    if kind == "cascade":
        chain = [m for m in arg.split(",") if m]
        best, total, used, err = 0.0, 0.0, [], None
        for model in chain:
            r = graded_run(pb, task_dir, "", model)
            total += cost_of(model)
            used.append(model)
            best = max(best, r["score"])
            err = r["error"]
            if r["score"] >= THRESHOLD:  # good enough — stop escalating
                break
        return {"score": best, "cost": total, "models": used, "error": err}
    raise ValueError(f"unknown policy: {policy!r} (use always:<model> or cascade:<m1,m2,...>)")


def eval_policy(pb: Pillbox, policy: str, refs: list[str], tmp: str, trials: int) -> dict:
    """Run every (task × trial) under `policy`; aggregate quality + cost."""
    dirs = {ref: _task_dir(pb, ref, tmp) for ref in refs}
    results = []
    for ref in refs:
        task = ref.split("/")[-1]
        for t in range(trials):
            r = run_policy(pb, policy, dirs[ref])
            results.append({"task": task, "trial": t, **r})
            print(f"  [{policy.split(':')[0]}] {task} → score={r['score']:.3f} "
                  f"cost={r['cost']:.1f} via {'+'.join(m.split('/')[-1] for m in r['models'])}", flush=True)
    q = mean(r["score"] for r in results) if results else 0.0
    c = mean(r["cost"] for r in results) if results else 0.0
    return {"policy": policy, "quality": round(q, 3), "cost": round(c, 2), "results": results}


@dataclass
class GhostConfig:
    pillbox: str
    task_set: str = "aider"
    evals_pillbox: str = "evals"
    trials: int = 1
    max_wait: int = 240
    runner_image: str = "pillbox-runner:l7"
    limit: int = 0
    local_model_port: int = 11434  # the libkrun host-forward port (local worker reachability)


def run_ghost(cfg: GhostConfig, policies: list[str]) -> dict:
    # The substrate wrapper wants its own Config shape; worker model is per-policy,
    # so leave it blank here (run_policy passes the routed model into graded_run).
    sub = SubstrateConfig(
        pillbox=cfg.pillbox, worker_model="", reflector_model="",
        task_set=cfg.task_set, evals_pillbox=cfg.evals_pillbox, trials=cfg.trials,
        max_wait=cfg.max_wait, runner_image=cfg.runner_image,
    )
    os.environ.setdefault("PILLBOX_LOCAL_MODEL_PORT", str(cfg.local_model_port))
    pb = Pillbox(sub)
    held = bookmarks(pb, cfg.task_set, "held-out")
    if not held:
        raise SystemExit(f"no frozen {cfg.task_set}/held-out/* in '{cfg.evals_pillbox}'")
    if cfg.limit:
        held = held[: cfg.limit]
    print(f"ghost: {len(held)} held-out tasks, trials={cfg.trials}, policies={policies}")
    arms = []
    with tempfile.TemporaryDirectory() as tmp:
        for p in policies:
            print(f"== {p} ==")
            arms.append(eval_policy(pb, p, held, tmp, cfg.trials))
    oracle = oracle_from_arms(arms)
    return {"task_set": cfg.task_set, "trials": cfg.trials, "n_held": len(held),
            "arms": arms, **({"oracle": oracle} if oracle else {})}


def oracle_from_arms(arms: list[dict]) -> dict | None:
    """The routing CEILING: per task, the cheapest model that MATCHES the best score any model
    achieved on it. Bounds the opportunity — if this barely beats always-frontier on cost, no
    router (learned or not) is worth building (the measure-before-optimize discipline). Also the
    DSPy router's training target: per-task `picks` = the label (the model the router should
    predict). Computed post-hoc from the always:* arms (>=2 needed); no extra runs."""
    always = [a for a in arms if a["policy"].startswith("always:")]
    if len(always) < 2:
        return None
    by_task: dict[str, dict[str, float]] = {}  # task → {model: mean score across trials}
    for a in always:
        model = a["policy"].split(":", 1)[1]
        per: dict[str, list[float]] = {}
        for r in a["results"]:
            per.setdefault(r["task"], []).append(r["score"])
        for task, scores in per.items():
            by_task.setdefault(task, {})[model] = mean(scores)
    q, c, picks = [], [], {}
    for task, scores in by_task.items():
        best = max(scores.values())
        winner = min((m for m, s in scores.items() if s >= best - 1e-9), key=cost_of)
        q.append(best)
        c.append(cost_of(winner))
        picks[task] = winner
    return {"policy": "oracle:cheapest-match-best", "quality": round(mean(q), 3),
            "cost": round(mean(c), 2), "picks": picks}


def report(out: dict):
    print(f"\n=== ghost router — held-out (n={out['n_held']}, trials={out['trials']}) ===")
    print(f"  {'policy':46} {'quality':>8} {'cost':>7} {'q/cost':>8}")
    for a in sorted(out["arms"], key=lambda x: -x["quality"]):
        qpc = a["quality"] / a["cost"] if a["cost"] else 0.0
        print(f"  {a['policy'][:46]:46} {a['quality']:>8.3f} {a['cost']:>7.2f} {qpc:>8.3f}")
    print("\nrouter wins if it matches the top quality at materially lower cost.")

    o = out.get("oracle")
    if o:
        # Best single model = the always:* arm with the top quality (the "frontier" play).
        always = [a for a in out["arms"] if a["policy"].startswith("always:")]
        best = max(always, key=lambda a: a["quality"])
        print(f"\n  {'⌜ ORACLE ceiling (perfect routing) ⌟':46} {o['quality']:>8.3f} {o['cost']:>7.2f}")
        gap = best["cost"] - o["cost"]
        print(f"opportunity: perfect routing reaches quality {o['quality']:.3f} at cost {o['cost']:.2f} vs "
              f"{best['policy'].split('/')[-1]} {best['quality']:.3f} @ {best['cost']:.2f}.")
        if gap <= 0.5:
            print("  → oracle cost ≈ frontier cost: routing CAN'T help much (most tasks need the top model). "
                  "Don't build the learned router on this set.")
        else:
            print(f"  → ~{gap:.1f} cost/task of headroom at equal-or-better quality — a learned router has room "
                  "to close. Worth the DSPy build; oracle 'picks' are its training labels.")


def main():
    ap = argparse.ArgumentParser(description="ghost — meta-harness router (v1)")
    ap.add_argument("--pillbox", default=os.environ.get("PILLBOX", "/tmp/pillbox-lk"))
    ap.add_argument("--policy", action="append", required=True,
                    help="repeatable: always:<model> | cascade:<m1,m2,...>")
    ap.add_argument("--task-set", default="aider")
    ap.add_argument("--evals-pillbox", default="evals")
    ap.add_argument("--trials", type=int, default=1)
    ap.add_argument("--max-wait", type=int, default=240)
    ap.add_argument("--runner-image", default=os.environ.get("PILLBOX_RUNNER_IMAGE", "pillbox-runner:l7"))
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--out", default="ghost-run.json")
    args = ap.parse_args()
    try:
        sys.stdout.reconfigure(line_buffering=True)
    except Exception:
        pass
    cfg = GhostConfig(
        pillbox=args.pillbox, task_set=args.task_set, evals_pillbox=args.evals_pillbox,
        trials=args.trials, max_wait=args.max_wait, runner_image=args.runner_image, limit=args.limit,
    )
    out = run_ghost(cfg, args.policy)
    with open(args.out, "w") as f:
        json.dump(out, f, indent=2)
    print(f"\nartifact → {args.out}")
    report(out)


if __name__ == "__main__":
    main()
