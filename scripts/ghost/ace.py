#!/usr/bin/env python3
"""ace.py — ghost's ACE loop (Agentic Context Engineering) over kypp.

ACE (arXiv 2510.04618) is a Generator→Reflector→Curator loop that grows a structured
playbook from execution feedback instead of rewriting a prompt — collapse-resistant
because it accrues incremental deltas. The insight (and the reason this is thin): every
stage is a kypp verb we already have, and the playbook IS kypp's governed memory.

  ACE stage            ghost does                         kypp verb
  ─────────            ──────────                         ─────────
  inject playbook      prepend the brief to the prompt    `kypp briefing`   ← the digest
  Generator            run the worker, grade it           pillbox run + score
  Reflector            mine the failure trajectory        `kypp capture --distill`
  Curator              dedup / promote / supersede        `kypp consolidate`
  helpful/harmful      attribute the score to seen claims `kypp usage` + the run score

So ACE bullets ARE kypp claims (ADD = a distilled claim, UPDATE/REMOVE = consolidate /
correct), and they inherit kypp's governance (authority, corroboration, staleness) —
the governance AxACE's flat playbook lacks. We do NOT add a second store.

What this loop measures (and what it does NOT): it tracks the held-out score as the
playbook grows over iterations — does accruing lessons help? It is NOT a quality-lift
claim against the σ̂ wall (the optimization gate, parked); the held-out signal is the
accrual question, kept honest by a fixed held-out split the loop never reflects on.

GAP (named, not hidden): the Curator's REMOVE-harmful needs a per-claim helpful/harmful
signal — credit-assignment #2, never built in kypp. The pieces exist here (kypp usage
records which claims a run saw; the run has a score), so this loop computes the
attribution ghost-side and REPORTS harmful candidates; acting on them (supersede) is
gated behind --prune-harmful (off by default — destructive + needs more than one round).

Usage:
  python3 ghost/ace.py --train aider --iters 3 --worker-model zai-coding-plan/glm-4.5-air \\
      --reflector-model zai-coding-plan/glm-5.1 --project ace-aider
  python3 ghost/ace.py --self-test     # attribution math, no agent
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from statistics import mean

# Reuse the proven pillbox substrate (run→drive→score, frozen tasks) from the gate rig.
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "eval"))
from gate import Pillbox, SubstrateConfig, _task_dir, bookmarks  # noqa: E402

HANDLE = re.compile(r"^([0-9a-f]{4,32})\b")  # kypp compact-line handle = leading hex


class Kypp:
    """Thin wrapper over the kypp CLI, scoped to one project + db. Best-effort like
    pillbox's own --memory: a kypp hiccup logs and the loop continues (never fails a run)."""

    def __init__(self, project: str, db: str | None):
        self.project = project
        self.env = {**os.environ, "KYPP_PROJECT": project}
        if db:
            self.env["KYPP_MEMORY_DB"] = db

    def _run(self, args, timeout=120):
        return subprocess.run(["kypp", *args], capture_output=True, text=True,
                              env=self.env, timeout=timeout)

    def briefing(self) -> tuple[str, list[str]]:
        """The current playbook digest + the handles it surfaced (the injection)."""
        p = self._run(["briefing", "--project", self.project], timeout=30)
        text = p.stdout.strip()
        if text.startswith("(no accepted"):  # kypp's empty-brief sentinel
            return "", []
        handles = [m.group(1) for line in text.splitlines() if (m := HANDLE.match(line.strip()))]
        return text, handles

    def reflect(self, sid: str):
        """Reflector: distil this session's failure trajectory into candidate claims."""
        p = self._run(["capture", "--session", sid, "--distill", "--project", self.project], timeout=300)
        if p.returncode != 0:
            print(f"    kypp capture(reflect) note: {p.stderr.strip()[:160]}", flush=True)

    def record_usage(self, sid: str, handles: list[str]):
        if not handles:
            return
        args = ["usage", "--record", "--session", sid, "--surface", "briefing"]
        for h in handles:
            args += ["--claim", h]
        self._run(args, timeout=30)

    def curate(self):
        """Curator: dedup / promote-corroborated / supersede. Governance applies."""
        p = self._run(["consolidate", "--project", self.project,
                       "--accept-corroboration", "2", "--semantic", "0.25"], timeout=120)
        if p.returncode != 0:
            print(f"    kypp consolidate note: {p.stderr.strip()[:160]}", flush=True)


def compose(brief: str, prompt: str) -> str:
    return f"## Project memory (kypp)\n{brief}\n\n## Task\n{prompt}" if brief else prompt


def generate(pb: Pillbox, ky: Kypp, task_dir: str, model: str, reflect: bool) -> dict:
    """One graded run with the current playbook injected. reflect=True also distils the
    trajectory (training) — held-out measurement passes reflect=False so it never feeds
    the playbook it's scoring. Reflect + usage-record happen BEFORE teardown (the §0 log
    is drained by drive's wait-idle, then `session rm` would take it)."""
    # The WHOLE body is in the try — incl. ky.briefing() (a subprocess that can time out)
    # and the prompt read — so the best-effort contract holds: any hiccup records an errored
    # cell, never crashes the loop. handles defaults [] so the except path can always return it.
    handles: list[str] = []
    try:
        brief, handles = ky.briefing()
        prompt = compose(brief, open(os.path.join(task_dir, "prompt.txt")).read())
        rubric = os.path.join(task_dir, "grader", "rubric.txt")
        use_rubric = os.path.exists(rubric)
        with tempfile.TemporaryDirectory() as ws:
            shutil.copytree(os.path.join(task_dir, "workspace"), ws, dirs_exist_ok=True)
            with pb.session(ws, model) as (sid, clone):
                pb.drive(sid, prompt)
                shutil.copytree(os.path.join(task_dir, "grader"), clone, dirs_exist_ok=True)
                v = pb.score(sid, clone, rubric if use_rubric else None,
                             None if use_rubric else "sh grade.sh")
                ky.record_usage(sid, handles)
                if reflect:
                    ky.reflect(sid)
        return {"score": float(v.get("score", 0.0)), "passed": bool(v.get("passed")),
                "seen": handles, "error": None}
    except Exception as e:  # noqa: BLE001 — record, never drop a task to a crash
        return {"score": 0.0, "passed": False, "seen": handles, "error": str(e)}


# min_seen: ignore claims with too little evidence; harmful_below 0.34 ≈ passes under a
# third of the time when present (a tunable suspicion threshold, not a hard verdict).
def attribute(records: list[dict], min_seen: int = 2, harmful_below: float = 0.34) -> dict:
    """Credit-assignment #2, ghost-side: per claim handle, pass-rate when it was in the
    brief. A handle seen often but mostly in FAILS is a harmful candidate. (Correlational,
    not causal — flagged for review, acted on only under --prune-harmful.)"""
    seen: dict[str, list[bool]] = {}
    for r in records:
        for h in r.get("seen", []):
            seen.setdefault(h, []).append(bool(r["passed"]))
    stats = {h: {"seen": len(v), "pass_rate": round(sum(v) / len(v), 3)} for h, v in seen.items()}
    harmful = [h for h, s in stats.items() if s["seen"] >= min_seen and s["pass_rate"] < harmful_below]
    return {"per_handle": stats, "harmful_candidates": harmful}


def eval_set(pb: Pillbox, ky: Kypp, refs: list[str], dirs: dict, model: str, reflect: bool, tag: str) -> list[dict]:
    out = []
    for ref in refs:
        r = generate(pb, ky, dirs[ref], model, reflect)
        r["task"] = ref.split("/")[-1]
        out.append(r)
        s = "ERR" if r["error"] else f"{r['score']:.3f}"
        print(f"    [{tag}] {r['task']} → {s}", flush=True)
    return out


def run_ace(args) -> dict:
    sub = SubstrateConfig(pillbox=args.pillbox, evals_pillbox=args.evals_pillbox,
                          max_wait=args.max_wait, runner_image=args.runner_image)
    pb = Pillbox(sub)
    ky = Kypp(args.project, args.db)
    train = bookmarks(pb, args.train, "train")
    held = bookmarks(pb, args.train, "held-out")
    if not train or not held:
        raise SystemExit(f"need frozen {args.train}/{{train,held-out}}/* in '{args.evals_pillbox}'")
    if args.limit:
        train, held = train[:args.limit], held[:args.limit]
    print(f"ace: train={len(train)} held={len(held)} iters={args.iters} worker={args.worker_model} project={args.project}")

    iterations = []
    with tempfile.TemporaryDirectory() as tmp:
        tdirs = {r: _task_dir(pb, r, tmp) for r in train}
        hdirs = {r: _task_dir(pb, r, tmp) for r in held}
        for it in range(args.iters):
            print(f"== iteration {it}: held-out measure (playbook as-is) ==")
            held_runs = eval_set(pb, ky, held, hdirs, args.worker_model, reflect=False, tag=f"held#{it}")
            held_q = round(mean(r["score"] for r in held_runs), 3) if held_runs else 0.0

            print(f"== iteration {it}: generator+reflector over train ==")
            train_runs = eval_set(pb, ky, train, tdirs, args.worker_model, reflect=True, tag=f"train#{it}")
            ky.curate()  # Curator
            attr = attribute(train_runs)
            if args.prune_harmful and attr["harmful_candidates"]:
                # Acting on these needs a kypp "reject/demote a claim BY HANDLE" verb.
                # `correct` is subject-keyed (asserts a new value for a SUBJECT) — it can't
                # target a specific harmful bullet by handle — so we report, never fake a
                # supersede. Deferred to ship-review: add `kypp reject <handle>` then wire it.
                print(f"    --prune-harmful: {len(attr['harmful_candidates'])} harmful candidate(s) "
                      f"flagged but NOT pruned (kypp has no reject-by-handle verb): {attr['harmful_candidates']}")

            iterations.append({"iter": it, "held_quality": held_q,
                               "harmful_candidates": attr["harmful_candidates"],
                               "attribution": attr["per_handle"]})
            print(f"  → iteration {it}: held_quality={held_q}", flush=True)

        # Each iter measures held at its START (playbook as-of-then), so the LAST iter's
        # train+curate produces a playbook nothing measures. One final measurement closes
        # that off-by-one — without it the accrual Δ silently drops the final (largest) round.
        print("== final held-out measure (after the last accrual round) ==")
        final_runs = eval_set(pb, ky, held, hdirs, args.worker_model, reflect=False, tag="held#final")
        final_q = round(mean(r["score"] for r in final_runs), 3) if final_runs else 0.0
        print(f"  → final held_quality={final_q}", flush=True)

    # curve = each iter's start measurement + the final post-loop one (iters+1 points).
    return {"project": args.project, "train_set": args.train, "iters": args.iters,
            "worker_model": args.worker_model, "iterations": iterations,
            "final_held_quality": final_q,
            "held_curve": [i["held_quality"] for i in iterations] + [final_q]}


def report(out: dict):
    print(f"\n=== ACE loop — {out['project']} (worker={out['worker_model']}) ===")
    print("  held-out quality per iteration (does the accruing playbook help?):")
    for i in out["iterations"]:
        h = i["harmful_candidates"]
        print(f"    iter {i['iter']}: {i['held_quality']:.3f}" + (f"   harmful: {h}" if h else ""))
    if "final_held_quality" in out:
        print(f"    final (after last round): {out['final_held_quality']:.3f}")
    curve = out["held_curve"]
    if len(curve) >= 2:
        d = curve[-1] - curve[0]
        verdict = ("accrual HELPS" if d > 0.05 else "accrual HURTS (context pollution?)" if d < -0.05
                   else "flat (no detectable accrual effect)")
        print(f"  Δ held-out (last − first) = {d:+.3f} → {verdict}")
        print("  NOTE: this is the accrual question, not a quality-lift claim vs the σ̂ wall.")


def self_test() -> int:
    """No agent: the attribution math (the helpful/harmful counter)."""
    recs = [
        {"seen": ["good", "bad"], "passed": True},
        {"seen": ["good", "bad"], "passed": True},
        {"seen": ["bad"], "passed": False},
        {"seen": ["good"], "passed": True},
    ]
    a = attribute(recs)
    ok = True
    if a["per_handle"]["good"]["pass_rate"] != 1.0:
        print(f"  ✗ good pass_rate {a['per_handle']['good']} (expect 1.0)"); ok = False
    if a["per_handle"]["bad"]["pass_rate"] != round(2 / 3, 3):
        print(f"  ✗ bad pass_rate {a['per_handle']['bad']} (expect 0.667)"); ok = False
    if a["harmful_candidates"] != []:  # bad@0.667 is above the 0.34 floor → not harmful
        print(f"  ✗ harmful {a['harmful_candidates']} (expect none at this threshold)"); ok = False
    a2 = attribute(recs, harmful_below=0.7)  # tighten → bad now flagged
    if "bad" not in a2["harmful_candidates"]:
        print(f"  ✗ tightened harmful should include 'bad': {a2['harmful_candidates']}"); ok = False
    print("self-test: " + ("PASS — attribution math sound" if ok else "FAIL"))
    return 0 if ok else 1


def main():
    ap = argparse.ArgumentParser(description="ghost ACE loop over kypp")
    ap.add_argument("--pillbox", default=os.environ.get("PILLBOX", "./target/debug/pillbox"))
    ap.add_argument("--train", default="aider", help="frozen task set name (<set>/{train,held-out}/*)")
    ap.add_argument("--project", default="ace", help="kypp project the playbook lives in")
    ap.add_argument("--db", default=os.environ.get("KYPP_MEMORY_DB"), help="kypp db (default: kypp's)")
    ap.add_argument("--worker-model", default="zai-coding-plan/glm-4.5-air")
    ap.add_argument("--reflector-model", default="zai-coding-plan/glm-5.1",
                    help="(distiller model is set via kypp's KYPP_DISTILL_MODEL env)")
    ap.add_argument("--iters", type=int, default=3)
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--max-wait", type=int, default=240)
    ap.add_argument("--evals-pillbox", default="evals")
    ap.add_argument("--runner-image", default=os.environ.get("PILLBOX_RUNNER_IMAGE", "pillbox-runner:l7"))
    ap.add_argument("--prune-harmful", action="store_true",
                    help="act on harmful candidates (supersede). Off by default — destructive.")
    ap.add_argument("--out", default="ace-run.json")
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args()
    try:
        sys.stdout.reconfigure(line_buffering=True)
    except Exception:
        pass
    if args.self_test:
        raise SystemExit(self_test())
    # gate._task_dir's pull runs with cwd=<dest>, so a relative binary path can't resolve
    # from there — make it absolute up front.
    args.pillbox = os.path.abspath(args.pillbox)
    out = run_ace(args)
    with open(args.out, "w") as f:
        json.dump(out, f, indent=2)
    print(f"\nartifact → {args.out}")
    report(out)


if __name__ == "__main__":
    main()
