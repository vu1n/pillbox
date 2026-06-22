#!/usr/bin/env python3
"""ace.py — ghost's ACE loop (Agentic Context Engineering) over kypp.

ACE (arXiv 2510.04618) is a Generator→Reflector→Curator loop that grows a structured
playbook from execution feedback instead of rewriting a prompt — collapse-resistant
because it accrues incremental deltas. The insight (and the reason this is thin): every
stage is a kypp verb we already have, and the playbook IS kypp's governed memory.

  ACE stage            ghost does                         kypp verb
  ─────────            ──────────                         ─────────
  inject playbook      prepend task-relevant claims       `kypp recall <task>` (compose)
  Generator            run the worker, grade it           pillbox run + score
  Reflector            mine the failure trajectory        `kypp capture --distill`
  Curator              dedup / promote / supersede        `kypp consolidate`
  helpful/harmful      attribute the score to seen claims `kypp usage` + the run score

Injection is task-conditioned `recall`, NOT `briefing` (dump-all): dumping the full
accepted store POLLUTES — a cheap model scores BELOW baseline on an unselected dump
(kypp handoff 2026-06-21, scripts/ghost/HANDOFF-kypp-kimi.md §2.1). Compose/select,
never dump. `--inject briefing` keeps the dump-all rung available for the ablation
(baseline vs compose vs dump-all = the handoff's open compose-lift test). Semantic
targeting needs the store's claims embedded AND KYPP_EMBED_MODEL set (recall embeds
the query); without it recall keyword-falls-back to generic top-claims.

So ACE bullets ARE kypp claims (ADD = a distilled claim, UPDATE/REMOVE = consolidate /
correct), and they inherit kypp's governance (authority, corroboration, staleness) —
the governance AxACE's flat playbook lacks. We do NOT add a second store.

What this loop measures (and what it does NOT): it tracks the held-out score as the
playbook grows over iterations — does accruing lessons help? It is NOT a quality-lift
claim against the σ̂ wall (the optimization gate, parked); the held-out signal is the
accrual question, kept honest by a fixed held-out split the loop never reflects on.

Curator REMOVE (credit-assignment #2): a per-claim helpful/harmful signal. kypp records
which claims a run saw (usage) and the run has a score, so this loop computes the
attribution ghost-side and flags harmful candidates; `kypp reject <handle>` (landed) does
the demote. Gated behind --prune-harmful (off by default — the attribution is
correlational, so don't auto-prune unsupervised on one round's evidence).

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
from math import comb
from statistics import mean, pstdev

# Reuse the proven pillbox substrate (run→drive→score, frozen tasks) from the gate rig.
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "eval"))
from gate import Pillbox, SubstrateConfig, _task_dir, bookmarks  # noqa: E402

HANDLE = re.compile(r"^([0-9a-f]{4,32})\b")  # kypp compact-line handle = leading hex

# The compose stage: an LLM builds a task-tailored packet from WIDE recall — relevance
# judgment + synthesis, dropping irrelevant claims (de-pollution), "NONE" when nothing fits.
# This is the orchestrator-composes decision (handoff's compose/Kimi-decode seat), not the
# worker self-recalling and not a mechanical top-k dump.
COMPOSE_PROMPT = """You assemble a context packet for a coding agent about to attempt ONE task.
Below are candidate lessons recalled from shared memory — SOME ARE IRRELEVANT to this task.
Keep ONLY lessons that genuinely apply to THIS task, synthesize them into a short actionable
brief, and DROP everything off-topic. Use only the candidate lessons — do not invent facts.
If nothing is relevant, output exactly: NONE

## Task
{task}

## Candidate lessons (from memory)
{candidates}

## Relevant brief (or NONE):
"""


def _llm_complete(prompt: str, model: str, timeout: int = 180) -> str:
    """Minimal one-shot LLM call for the composer. `ollama:<model>` → local HTTP (no deps).
    Model-agnostic seam: add claude/codex/opencode schemes here to point the composer elsewhere."""
    if model.startswith("ollama:"):
        import urllib.request
        body = json.dumps({"model": model[len("ollama:"):], "prompt": prompt,
                           "stream": False, "options": {"temperature": 0}}).encode()
        req = urllib.request.Request("http://localhost:11434/api/generate", data=body,
                                     headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read())["response"]
    raise ValueError(f"composer scheme not supported: {model!r} (use 'ollama:<model>')")


class Kypp:
    """Thin wrapper over the kypp CLI, scoped to one project + db. Best-effort like
    pillbox's own --memory: a kypp hiccup logs and the loop continues (never fails a run)."""

    def __init__(self, project: str, db: str | None, embed_model: str | None = None,
                 recall_candidates: bool = False, composer_model: str = "", compose_wide: int = 15):
        self.project = project
        self.recall_candidates = recall_candidates
        self.composer_model = composer_model
        self.compose_wide = compose_wide
        self.env = {**os.environ, "KYPP_PROJECT": project}
        if db:
            self.env["KYPP_MEMORY_DB"] = db
        # recall embeds the query for semantic ranking only when an embedder is wired;
        # without this it keyword-falls-back, so the compose arm silently degrades.
        if embed_model:
            self.env["KYPP_EMBED_MODEL"] = embed_model

    def _run(self, args, timeout=120):
        return subprocess.run(["kypp", *args], capture_output=True, text=True,
                              env=self.env, timeout=timeout)

    @staticmethod
    def _parse(stdout: str) -> tuple[str, list[str]]:
        """Compact handle-lines → (brief text, handles). Shared by recall + briefing;
        both emit the same format and the same `(no …)` empty sentinel."""
        text = stdout.strip()
        if not text or text.startswith("(no "):
            return "", []
        handles = [m.group(1) for line in text.splitlines() if (m := HANDLE.match(line.strip()))]
        return text, handles

    def recall(self, query: str, limit: int) -> tuple[str, list[str]]:
        """Task-conditioned injection: the top-k claims relevant to THIS task, not the
        whole store — the compose/select rung (see module docstring on pollution)."""
        a = ["recall", query, "--limit", str(limit), "--project", self.project]
        if self.recall_candidates:  # include unaccepted — avoids an empty brief on a corroboration-starved store
            a.append("--candidates")
        p = self._run(a, timeout=30)
        return self._parse(p.stdout)

    def briefing(self) -> tuple[str, list[str]]:
        """Dump-all: the full accepted playbook. Pollutes a cheap model — kept only as
        the ablation's dump-all rung; prefer recall/compose for the live loop."""
        p = self._run(["briefing", "--project", self.project], timeout=30)
        return self._parse(p.stdout)

    def compose_packet(self, task_prompt: str) -> tuple[str, list[str]]:
        """The compose stage: recall WIDE, then an LLM builds a task-tailored packet from the
        candidates (relevance judgment + synthesis, "NONE" → clean no-injection). Returns the
        composed brief + the candidate handles (usage provenance is over what was SHOWN to the
        composer). Best-effort: a composer hiccup falls back to the raw wide recall, never crashes."""
        cand, handles = self.recall(task_prompt[:400], self.compose_wide)
        if not cand:
            return "", []
        try:
            packet = _llm_complete(
                COMPOSE_PROMPT.format(task=task_prompt[:1500], candidates=cand),
                self.composer_model).strip()
        except Exception as e:  # noqa: BLE001 — composer is best-effort; fall back to raw recall
            print(f"    compose note: {str(e)[:160]} — falling back to raw recall", flush=True)
            return cand, handles
        if not packet or packet.upper().startswith("NONE"):
            return "", handles  # composer judged nothing relevant — de-pollution by construction
        return packet, handles

    def reflect(self, sid: str):
        """Reflector: distil this session's failure trajectory into candidate claims."""
        p = self._run(["capture", "--session", sid, "--distill", "--project", self.project], timeout=300)
        if p.returncode != 0:
            print(f"    kypp capture(reflect) note: {p.stderr.strip()[:160]}", flush=True)

    def record_usage(self, sid: str, handles: list[str], surface: str = "recall"):
        if not handles:
            return
        args = ["usage", "--record", "--session", sid, "--surface", surface]
        for h in handles:
            args += ["--claim", h]
        self._run(args, timeout=30)

    def curate(self, accept: int = 2):
        """Curator: dedup / promote-corroborated / supersede. Governance applies. `accept` =
        corroboration bar (≥N independent sessions agree). Lower it for a small/diverse train
        set where ≥2 starves the store to empty (handoff §2.3); 1 accepts single-session claims."""
        p = self._run(["consolidate", "--project", self.project,
                       "--accept-corroboration", str(accept), "--semantic", "0.25"], timeout=120)
        if p.returncode != 0:
            print(f"    kypp consolidate note: {p.stderr.strip()[:160]}", flush=True)

    def reject(self, handle: str, reason: str):
        """Curator REMOVE: demote a harmful claim by handle (kypp reject — status=rejected, dropped
        from recall/briefing, row preserved). Handle is global, so no --project; env scopes the db."""
        p = self._run(["reject", handle, "--reason", reason], timeout=30)
        if p.returncode != 0:
            print(f"    kypp reject note: {p.stderr.strip()[:160]}", flush=True)


def compose(brief: str, prompt: str) -> str:
    return f"## Project memory (kypp)\n{brief}\n\n## Task\n{prompt}" if brief else prompt


def generate(pb: Pillbox, ky: Kypp, task_dir: str, model: str, reflect: bool,
             inject: str = "recall", limit: int = 5) -> dict:
    """One graded run with the current playbook injected. reflect=True also distils the
    trajectory (training) — held-out measurement passes reflect=False so it never feeds
    the playbook it's scoring. Reflect + usage-record happen BEFORE teardown (the §0 log
    is drained by drive's wait-idle, then `session rm` would take it).

    inject: "recall" = task-conditioned compose (default), "briefing" = dump-all
    (ablation), "none" = baseline (no memory)."""
    # The WHOLE body is in the try — incl. the kypp subprocess (can time out) and the
    # prompt read — so the best-effort contract holds: any hiccup records an errored cell,
    # never crashes the loop. handles defaults [] so the except path can always return it.
    handles: list[str] = []
    try:
        task_prompt = open(os.path.join(task_dir, "prompt.txt")).read()
        if inject == "recall":  # query with the task prompt head (mirrors run-task.sh's recall arm)
            brief, handles = ky.recall(task_prompt[:400], limit)
        elif inject == "compose":  # orchestrator LLM builds a tailored packet from wide recall
            brief, handles = ky.compose_packet(task_prompt)
        elif inject == "briefing":
            brief, handles = ky.briefing()
        else:  # "none" — baseline, no injection
            brief, handles = "", []
        prompt = compose(brief, task_prompt)
        rubric = os.path.join(task_dir, "grader", "rubric.txt")
        use_rubric = os.path.exists(rubric)
        with tempfile.TemporaryDirectory() as ws:
            shutil.copytree(os.path.join(task_dir, "workspace"), ws, dirs_exist_ok=True)
            with pb.session(ws, model) as (sid, clone):
                pb.drive(sid, prompt)
                shutil.copytree(os.path.join(task_dir, "grader"), clone, dirs_exist_ok=True)
                v = pb.score(sid, clone, rubric if use_rubric else None,
                             None if use_rubric else "sh grade.sh")
                ky.record_usage(sid, handles, surface=inject)
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


def eval_set(pb: Pillbox, ky: Kypp, refs: list[str], dirs: dict, model: str, reflect: bool, tag: str,
             inject: str = "recall", limit: int = 5) -> list[dict]:
    out = []
    for ref in refs:
        r = generate(pb, ky, dirs[ref], model, reflect, inject, limit)
        r["task"] = ref.split("/")[-1]
        out.append(r)
        s = "ERR" if r["error"] else f"{r['score']:.3f}"
        print(f"    [{tag}] {r['task']} → {s}", flush=True)
    return out


def _setup(args):
    """Shared substrate + kypp wiring + frozen-task resolution for all three modes."""
    sub = SubstrateConfig(pillbox=args.pillbox, evals_pillbox=args.evals_pillbox,
                          max_wait=args.max_wait, runner_image=args.runner_image)
    pb = Pillbox(sub)
    ky = Kypp(args.project, args.db, args.embed_model, args.recall_candidates,
              args.composer_model, args.compose_wide)
    if args.inject in ("recall", "compose") and not args.embed_model:
        print("ace: warning — --inject recall/compose without an embed model (KYPP_EMBED_MODEL unset); "
              "recall will keyword-fall-back to generic top-claims, NOT semantic targeting.", flush=True)
    train = bookmarks(pb, args.train, "train")
    held = bookmarks(pb, args.train, "held-out")
    if not train or not held:
        raise SystemExit(f"need frozen {args.train}/{{train,held-out}}/* in '{args.evals_pillbox}'")
    if args.tasks:  # name filter (e.g. the headroom subset) — applies to whichever split the mode uses
        want = {s.strip() for s in args.tasks.split(",") if s.strip()}
        train = [r for r in train if r.split("/")[-1] in want]
        held = [r for r in held if r.split("/")[-1] in want]
    if args.limit:
        train, held = train[:args.limit], held[:args.limit]
    return pb, ky, train, held


def run_build(args) -> dict:
    """Build the store: solve each train task `trials` times and reflect (inject=none — we
    POPULATE the store here, we don't inject while building). Repeated trials let the SAME
    lesson corroborate (≥accept) without lowering quality — the handoff-sanctioned alternative
    to accept=1 on a diverse set. No held-out measurement. seed-xor-donate: TRAIN only."""
    pb, ky, train, held = _setup(args)
    build_tasks = held if args.build_split == "held-out" else train  # related-task: build from a held-out sibling
    print(f"ace build: tasks={len(build_tasks)} ({args.build_split}) trials={args.trials} "
          f"accept={args.accept_corroboration} worker={args.worker_model} project={args.project}")
    with tempfile.TemporaryDirectory() as tmp:
        tdirs = {r: _task_dir(pb, r, tmp) for r in build_tasks}
        train = build_tasks
        for t in range(args.trials):
            eval_set(pb, ky, train, tdirs, args.worker_model, reflect=True, tag=f"build#{t}", inject="none")
        ky.curate(args.accept_corroboration)
    brief, handles = ky.briefing()
    print(f"ace build: store now surfaces {len(handles)} accepted claim(s) at briefing")
    return {"mode": "build", "project": args.project, "train_set": args.train, "trials": args.trials,
            "accept_corroboration": args.accept_corroboration, "accepted_handles": handles}


def run_measure(args) -> dict:
    """Measure (NO train/reflect): each held-out task × `trials` under --inject, against the
    EXISTING store. Per-task mean/σ̂ + optional JSONL records for paired-stats. The clean
    compose-lift arm — run once per inject mode (none / recall / briefing) vs the SAME store."""
    pb, ky, _, held = _setup(args)
    print(f"ace measure: held={len(held)} trials={args.trials} inject={args.inject} "
          f"worker={args.worker_model} project={args.project}")
    fh = open(args.records, "a") if args.records else None
    per_task = {}
    with tempfile.TemporaryDirectory() as tmp:
        hdirs = {r: _task_dir(pb, r, tmp) for r in held}
        for ref in held:
            task = ref.split("/")[-1]
            scores = []
            for t in range(args.trials):
                r = generate(pb, ky, hdirs[ref], args.worker_model, reflect=False,
                             inject=args.inject, limit=args.inject_limit)
                sc = 0.0 if r["error"] else float(r["score"])
                scores.append(sc)
                print(f"    [measure {args.inject} {task} {t+1}/{args.trials}] → "
                      f"{'ERR' if r['error'] else f'{sc:.3f}'}", flush=True)
                if fh:
                    fh.write(json.dumps({"task": task, "inject": args.inject, "trial": t,
                                         "score": sc, "seen": r.get("seen", []), "error": r["error"]}) + "\n")
                    fh.flush()
            per_task[task] = {"n": len(scores), "mean": round(mean(scores), 3),
                              "sigma": round(pstdev(scores), 3), "scores": scores}
    if fh:
        fh.close()
    overall = round(mean([s for v in per_task.values() for s in v["scores"]]), 3) if per_task else 0.0
    print(f"\n=== measure ({args.inject}) — per-task mean/σ̂ (n={args.trials}) ===")
    for task, v in per_task.items():
        print(f"  {task:18} mean={v['mean']:.3f} σ̂={v['sigma']:.3f}  {v['scores']}")
    print(f"  overall mean = {overall:.3f}")
    return {"mode": "measure", "project": args.project, "inject": args.inject,
            "trials": args.trials, "per_task": per_task, "overall_mean": overall}


def pass_at_k(n: int, c: int, k: int) -> float:
    """Unbiased pass@k (HumanEval / Codex): P(≥1 of k sampled from n attempts, c correct, passes).
    The SWARM metric — best-of-k of independent diverse attempts. Stable where single-attempt mean
    is not (it's a proportion near a ceiling), which is how the swarm frame escapes the variance wall."""
    k = min(k, n)
    if n - c < k:
        return 1.0
    return 1.0 - comb(n - c, k) / comb(n, k)


def run_swarm(args) -> dict:
    """Best-of-k SWARM measure: `--trials` (=N) diverse attempts per held-out task under --inject,
    aggregated as pass@k. The swarm beats solo when pass@k ≫ pass@1; the cheap worker's bistability
    IS the diversity best-of-k exploits (no temperature needed). Cost proxy = k worker-runs.
    INTEGRITY: grader stays hidden (gate injects post-solve); memory must be cross-task (seed-XOR-donate)."""
    pb, ky, _, held = _setup(args)
    N = args.trials
    ks = sorted({k for k in (1, 2, 3, 5, args.swarm_k, N) if 1 <= k <= N})
    print(f"ace swarm: held={len(held)} N={N} inject={args.inject} pass@{ks} "
          f"thresh={args.pass_threshold} worker={args.worker_model} project={args.project}")
    fh = open(args.records, "a") if args.records else None
    per_task = {}
    with tempfile.TemporaryDirectory() as tmp:
        hdirs = {r: _task_dir(pb, r, tmp) for r in held}
        for ref in held:
            task = ref.split("/")[-1]
            scores = []
            for t in range(N):
                r = generate(pb, ky, hdirs[ref], args.worker_model, reflect=False,
                             inject=args.inject, limit=args.inject_limit)
                sc = 0.0 if r["error"] else float(r["score"])
                passed = sc >= args.pass_threshold
                scores.append(sc)
                print(f"    [swarm {args.inject} {task} {t+1}/{N}] → "
                      f"{'ERR' if r['error'] else f'{sc:.3f}'}{' ✓' if passed else ''}", flush=True)
                if fh:
                    fh.write(json.dumps({"task": task, "inject": args.inject, "trial": t, "score": sc,
                                         "passed": passed, "seen": r.get("seen", []), "error": r["error"]}) + "\n")
                    fh.flush()
            c = sum(1 for s in scores if s >= args.pass_threshold)
            per_task[task] = {"n": N, "c": c, "best": round(max(scores), 3) if scores else 0.0,
                              "pass_at": {k: round(pass_at_k(N, c, k), 3) for k in ks}}
    if fh:
        fh.close()
    pooled = {k: round(mean(v["pass_at"][k] for v in per_task.values()), 3) for k in ks} if per_task else {}
    hdr = "  ".join(f"@{k}" for k in ks)
    print(f"\n=== swarm ({args.inject}) — pass@k (pass = score ≥ {args.pass_threshold}) ===")
    print("  %-16s %5s  %s" % ("task", "c/n", hdr))
    for task, v in per_task.items():
        print("  %-16s %4s  %s" % (task, f"{v['c']}/{v['n']}", "  ".join(f"{v['pass_at'][k]:.2f}" for k in ks)))
    if pooled:
        print("  %-16s %5s  %s" % ("POOLED", "", "  ".join(f"{pooled[k]:.2f}" for k in ks)))
    print(f"  cost proxy: swarm@k = k worker-runs (pass@{args.swarm_k} costs {args.swarm_k}× a solo attempt)")
    return {"mode": "swarm", "project": args.project, "inject": args.inject, "N": N,
            "pass_threshold": args.pass_threshold, "ks": ks, "per_task": per_task, "pooled": pooled}


def run_ace(args) -> dict:
    pb, ky, train, held = _setup(args)
    print(f"ace: train={len(train)} held={len(held)} iters={args.iters} worker={args.worker_model} project={args.project}")

    iterations = []
    with tempfile.TemporaryDirectory() as tmp:
        tdirs = {r: _task_dir(pb, r, tmp) for r in train}
        hdirs = {r: _task_dir(pb, r, tmp) for r in held}
        for it in range(args.iters):
            print(f"== iteration {it}: held-out measure (playbook as-is) ==")
            held_runs = eval_set(pb, ky, held, hdirs, args.worker_model, reflect=False, tag=f"held#{it}",
                                 inject=args.inject, limit=args.inject_limit)
            held_q = round(mean(r["score"] for r in held_runs), 3) if held_runs else 0.0

            print(f"== iteration {it}: generator+reflector over train ==")
            train_runs = eval_set(pb, ky, train, tdirs, args.worker_model, reflect=True, tag=f"train#{it}",
                                  inject=args.inject, limit=args.inject_limit)
            ky.curate(args.accept_corroboration)  # Curator
            attr = attribute(train_runs)
            if args.prune_harmful and attr["harmful_candidates"]:
                # Curator REMOVE — now real (kypp reject <handle> landed): demote each harmful
                # claim out of recall/briefing. Still gated off by default: the attribution is
                # correlational, so one round's evidence shouldn't auto-prune unsupervised.
                print(f"    --prune-harmful: rejecting {len(attr['harmful_candidates'])} harmful candidate(s): "
                      f"{attr['harmful_candidates']}")
                for h in attr["harmful_candidates"]:
                    ky.reject(h, f"ACE attribution ({args.train}): correlated with failed runs")

            iterations.append({"iter": it, "held_quality": held_q,
                               "harmful_candidates": attr["harmful_candidates"],
                               "attribution": attr["per_handle"]})
            print(f"  → iteration {it}: held_quality={held_q}", flush=True)

        # Each iter measures held at its START (playbook as-of-then), so the LAST iter's
        # train+curate produces a playbook nothing measures. One final measurement closes
        # that off-by-one — without it the accrual Δ silently drops the final (largest) round.
        print("== final held-out measure (after the last accrual round) ==")
        final_runs = eval_set(pb, ky, held, hdirs, args.worker_model, reflect=False, tag="held#final",
                              inject=args.inject, limit=args.inject_limit)
        final_q = round(mean(r["score"] for r in final_runs), 3) if final_runs else 0.0
        print(f"  → final held_quality={final_q}", flush=True)

    # curve = each iter's start measurement + the final post-loop one (iters+1 points).
    return {"project": args.project, "train_set": args.train, "iters": args.iters,
            "worker_model": args.worker_model, "inject": args.inject, "iterations": iterations,
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
    # pass@k estimator (swarm metric)
    for n, c, k, want in [(10, 0, 5, 0.0), (10, 10, 5, 1.0), (10, 5, 1, 0.5),
                          (10, 5, 2, 0.778), (10, 1, 5, 0.5)]:
        got = round(pass_at_k(n, c, k), 3)
        if got != want:
            print(f"  ✗ pass_at_k({n},{c},{k})={got} (expect {want})"); ok = False
    print("self-test: " + ("PASS — attribution + pass@k math sound" if ok else "FAIL"))
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
    ap.add_argument("--inject", choices=["recall", "compose", "briefing", "none"], default="recall",
                    help="memory injection: recall=mechanical top-k, compose=LLM-built packet "
                         "(orchestrator composes from wide recall), briefing=dump-all (pollution baseline), none=baseline")
    ap.add_argument("--inject-limit", type=int, default=5, help="top-k claims for --inject recall")
    ap.add_argument("--composer-model", default="ollama:qwen3.6:35b-a3b-coding-nvfp4",
                    help="LLM that builds the --inject compose packet (model-agnostic: ollama:<model>; "
                         "cheap is fine at small store, Kimi at scale)")
    ap.add_argument("--compose-wide", type=int, default=15, help="candidates recalled wide before the composer selects")
    ap.add_argument("--build-split", choices=["train", "held-out"], default="train",
                    help="build: which split to reflect on (held-out = build from a sibling for related-task transfer)")
    ap.add_argument("--embed-model", default=os.environ.get("KYPP_EMBED_MODEL"),
                    help="embedder for semantic recall (e.g. nomic-embed-text); "
                         "default $KYPP_EMBED_MODEL. Unset → recall keyword-falls-back.")
    ap.add_argument("--mode", choices=["loop", "build", "measure", "swarm"], default="loop",
                    help="loop=accrual curve (default); build=populate store from train; "
                         "measure=held-out × --trials mean/σ̂; swarm=best-of-k pass@k (the swarm thesis metric)")
    ap.add_argument("--swarm-k", type=int, default=5, help="swarm: headline k for pass@k (also reports 1/2/3/N)")
    ap.add_argument("--pass-threshold", type=float, default=1.0,
                    help="swarm: score ≥ this counts as a pass (default 1.0 = full rubric; partial ≠ solved)")
    ap.add_argument("--trials", type=int, default=1,
                    help="measure: trials per held task; build: solve-reps per train task (for corroboration)")
    ap.add_argument("--accept-corroboration", type=int, default=2,
                    help="consolidate corroboration bar (≥N sessions). 1 = accept single-session claims "
                         "(needed on a small/diverse train set, else the store starves empty)")
    ap.add_argument("--recall-candidates", action="store_true",
                    help="recall includes unaccepted candidates — avoids an empty brief on a starved store")
    ap.add_argument("--records", default=None, help="measure: append per-(task,trial) JSONL here for paired-stats")
    ap.add_argument("--tasks", default="", help="comma-sep task basenames to restrict to (e.g. the headroom subset)")
    ap.add_argument("--iters", type=int, default=3)
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--max-wait", type=int, default=240)
    ap.add_argument("--evals-pillbox", default="evals")
    ap.add_argument("--runner-image", default=os.environ.get("PILLBOX_RUNNER_IMAGE", "pillbox-runner:dev"))
    ap.add_argument("--prune-harmful", action="store_true",
                    help="reject (kypp reject) harmful candidates. Off by default — correlational, destructive.")
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
    out = {"loop": run_ace, "build": run_build, "measure": run_measure, "swarm": run_swarm}[args.mode](args)
    with open(args.out, "w") as f:
        json.dump(out, f, indent=2)
    print(f"\nartifact → {args.out}")
    if args.mode == "loop":
        report(out)


if __name__ == "__main__":
    main()
