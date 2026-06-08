#!/usr/bin/env python3
"""gate.py — the optimization-gate eval, as a real tool (replaces the bash pile).

WHAT it measures: does an instruction layer lift a small WORKER model over baseline,
on a held-out split, via pillbox's verifiable rubric-graded reward? Three arms:
  baseline — worker model, task prompt only.
  ace      — worker + a curated playbook prepended.  ⚠ STATIC-PLAYBOOK PROXY, not
             real ACE (arXiv 2510.04618): true ACE *evolves* the playbook at runtime
             from execution feedback via a Reflector, with incremental non-collapsing
             deltas. The 2026-06-08 deep-research pass elevated ACE to a mandatory arm
             (beats GEPA +11.9% on AppWorld; +14.8% with NO labels). DO NOT read a
             null "ace" result here as "ACE doesn't help" — this arm doesn't implement
             ACE's mechanism. Upgrade to a real evolving-context arm before trusting
             any ACE verdict (gated behind the deterministic-worker retry — see the
             optimization-layer-verdict memory and docs/optimization-eval-family.md §6).
  gepa     — worker + a profile a (frontier) REFLECTOR distilled from train failures.
Teacher→student: --worker-model and --reflector-model are separate, so the cheap
worker does the volume and the frontier model is spent only at distill time.

WHY a tool, not bash: real timeouts, per-session teardown (no broad pkill that nukes
other sessions), structured data (no TSV/env soup), and a durable, resumable run
artifact (config + seeds + per-task/per-arm scores) — an artifact you can diff, not a
log you screenshot.

pillbox's CLI is the substrate (run/send/wait-idle/score/pull); this is the consumer.
It is the TEST rig, not the production meta-harness — it wraps the optimizer (the
distill+run loop) in a measurement protocol (splits, arms, lift) to decide go/no-go.

Usage:
  python3 gate.py --worker-model zai-coding-plan/glm-4.5-air \\
                  --reflector-model zai-coding-plan/glm-5.1 \\
                  --task-set aider --trials 1 --out /tmp/gate-run.json
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import tempfile
from concurrent.futures import ThreadPoolExecutor, as_completed
from contextlib import contextmanager
from dataclasses import asdict, dataclass, field
from statistics import mean

REFLECT_PROMPT = (
    "You are improving the INSTRUCTION PROFILE for a coding agent. failures/ holds tasks "
    "the agent just FAILED — each shows the task, what it produced, its tool trajectory, and "
    "the grader's per-criterion feedback (which checks failed). Diagnose the PATTERNS and write "
    "an improved, GENERAL profile into PROFILE.md: a short list of bullets that will help on "
    "future UNSEEN tasks. Rules: keep it general (do NOT hardcode answers/task specifics); "
    "concrete and simple enough for a small model to follow; under ~12 bullets. Edit PROFILE.md only."
)


# The 5 fields the Pillbox substrate actually reads (__init__/_json/pull/score + bookmarks).
# Split out so consumers that only need the substrate (ghost.py, ace.py) construct THIS
# and don't stub the gate's arm-protocol fields. gate's Config extends it.
@dataclass
class SubstrateConfig:
    pillbox: str
    evals_pillbox: str = "evals"
    max_wait: int = 240
    in_sandbox: bool = False
    runner_image: str = "pillbox-runner:l7"


@dataclass
class Config(SubstrateConfig):
    # gate's three-arm protocol fields (worker/reflector/playbook/…) — NOT read by the
    # substrate. worker/reflector default "" only to satisfy dataclass field ordering
    # (base has defaulted fields); main() requires them via argparse, so the default never bites.
    worker_model: str = ""
    reflector_model: str = ""
    task_set: str = "aider"
    trials: int = 1
    playbook: str = ""
    out: str = "gate-run.json"
    parallel: int = 1   # serial by default; >1 only for LOCAL models (hosted plans throttle concurrent reqs → corrupts scores)
    limit: int = 0      # 0 = all; else cap tasks per split (fast iteration)


class PillboxError(RuntimeError):
    pass


class Pillbox:
    """Thin, structured wrapper over the pillbox CLI. Every call has a real timeout;
    sessions are torn down per-id in `session()` (no broad pkill)."""

    def __init__(self, cfg: Config):
        self.cfg = cfg
        self.env = {
            **os.environ,
            "PILLBOX_BACKEND": "libkrun",
            "PILLBOX_RUNNER_IMAGE": cfg.runner_image,
        }

    def _json(self, args, timeout, cwd=None):
        p = subprocess.run(
            [self.cfg.pillbox, *args], capture_output=True, text=True, env=self.env,
            timeout=timeout, cwd=cwd,
        )
        try:
            return json.loads(p.stdout)
        except json.JSONDecodeError as e:
            raise PillboxError(f"{' '.join(args[:3])}: bad JSON ({e}); stderr={p.stderr[:200]}")

    def pull(self, bookmark: str, dest: str):
        subprocess.run(
            [self.cfg.pillbox, "--pillbox", self.cfg.evals_pillbox, "pull", "--bookmark", bookmark],
            cwd=dest, capture_output=True, text=True, env=self.env, timeout=120, check=True,
        )

    def start(self, workspace: str, model: str) -> tuple[str, str]:
        """run --json returns once the session reparents; → (sid, result-workspace path)."""
        d = self._json(
            ["run", "--agent", "opencode", "--json", "--workspace", workspace, "--model", model],
            timeout=120,
        )
        sid = d["session"]["id"]
        ws = self._json(["session", "info", sid, "--json"], timeout=30)["session"].get("workspace", "")
        if not ws:
            raise PillboxError(f"session {sid}: no result-workspace (backend not libkrun?)")
        return sid, ws

    def drive(self, sid: str, prompt: str):
        subprocess.run([self.cfg.pillbox, "session", "send", sid, prompt],
                       capture_output=True, env=self.env, timeout=60)
        try:
            subprocess.run([self.cfg.pillbox, "session", "wait-idle", sid, "--timeout",
                            str(self.cfg.max_wait)], capture_output=True, env=self.env,
                           timeout=self.cfg.max_wait + 60)
        except subprocess.TimeoutExpired:
            pass  # turn ran long; grade whatever landed

    def score(self, sid: str, clone: str, rubric: str | None, cmd: str | None) -> dict:
        args = ["session", "score", sid, "--workspace", clone, "--json"]
        args += ["--rubric", rubric] if rubric else ["--cmd", cmd]
        if self.cfg.in_sandbox:
            args.append("--in-sandbox")
        return self._json(args, timeout=self.cfg.max_wait + 60)

    def rm(self, sid: str):
        subprocess.run([self.cfg.pillbox, "session", "rm", sid],
                       capture_output=True, env=self.env, timeout=60)

    @contextmanager
    def session(self, workspace: str, model: str):
        """Start a session; always `rm` it (per-id teardown) — even on error/timeout."""
        sid = ws = None
        try:
            sid, ws = self.start(workspace, model)
            yield sid, ws
        finally:
            if sid:
                self.rm(sid)


def _task_dir(pb: Pillbox, ref: str, tmp: str) -> str:
    """A frozen bookmark `<set>/<split>/<id>` → a pulled task dir under tmp."""
    d = os.path.join(tmp, ref.replace("/", "_"))
    os.makedirs(d, exist_ok=True)
    pb.pull(ref, d)
    return d


def graded_run(pb: Pillbox, task_dir: str, profile: str, model: str) -> dict:
    """Run the worker on one task with `profile` prepended, grade it. Always returns a
    result dict with a numeric score — failures are recorded, never silently dropped."""
    prompt = open(os.path.join(task_dir, "prompt.txt")).read()
    if profile:
        prompt = profile + "\n\n" + prompt
    rubric = os.path.join(task_dir, "grader", "rubric.txt")
    with tempfile.TemporaryDirectory() as ws:
        shutil.copytree(os.path.join(task_dir, "workspace"), ws, dirs_exist_ok=True)
        try:
            with pb.session(ws, model) as (sid, clone):
                pb.drive(sid, prompt)
                # Inject the hidden grader into the edited clone, THEN score — the agent
                # never saw the checks.
                shutil.copytree(os.path.join(task_dir, "grader"), clone, dirs_exist_ok=True)
                use_rubric = os.path.exists(rubric)
                v = pb.score(sid, clone, rubric if use_rubric else None,
                             None if use_rubric else "sh grade.sh")
            return {"score": float(v.get("score", 0.0)), "passed": bool(v.get("passed")),
                    "criteria": v.get("criteria", []), "feedback": v.get("feedback", ""), "error": None}
        except (PillboxError, subprocess.TimeoutExpired, subprocess.CalledProcessError) as e:
            return {"score": 0.0, "passed": False, "criteria": [], "feedback": "", "error": str(e)}


def distill(pb: Pillbox, failures: list[dict], reflector_model: str) -> str:
    """The GEPA reflect step — a FRONTIER reflector writes a profile from worker failures.
    Returns the profile text (empty if the reflector produced nothing)."""
    if not failures:
        return ""
    with tempfile.TemporaryDirectory() as ws:
        fdir = os.path.join(ws, "failures")
        os.makedirs(fdir)
        for f in failures:
            with open(os.path.join(fdir, f"{f['task']}.md"), "w") as fh:
                fh.write(f"## TASK {f['task']}\n\n## GRADER FEEDBACK\n{f['feedback']}\n")
        open(os.path.join(ws, "PROFILE.md"), "w").close()
        try:
            with pb.session(ws, reflector_model) as (sid, clone):
                pb.drive(sid, REFLECT_PROMPT)
                prof = os.path.join(clone, "PROFILE.md")
                text = open(prof).read().strip() if os.path.exists(prof) else ""
                if not text:
                    # Loud: the reflector session ran but wrote no profile (didn't
                    # follow the instruction, timed out mid-write, or empty file).
                    # Caller must NOT treat a profile-less GEPA arm as a real result.
                    print(f"  distill: reflector wrote NO profile (PROFILE.md "
                          f"{'missing' if not os.path.exists(prof) else 'empty'}) — GEPA arm will be skipped")
                return text
        except (PillboxError, subprocess.TimeoutExpired) as e:
            print(f"  distill failed: {e}")
            return ""


def eval_arm(pb: Pillbox, label: str, refs: list[str], profile: str, model: str,
             tmp: str, trials: int, parallel: int = 4, capture_failures: bool = False) -> dict:
    """Score `model`+`profile` over `refs` (×trials), `parallel` tasks at once. Tasks are
    independent (own VM + own workspace clone), so they fan out; teardown stays per-`sid`.
    Returns per-task results + mean + (optionally) the failure reports for the reflector."""
    # Pre-pull each ref once, serially (no VM, fast) — graded_run copies from it read-only,
    # so concurrent jobs can share the pulled dir without a pull race.
    dirs = {ref: _task_dir(pb, ref, tmp) for ref in refs}
    jobs = [(ref, t) for ref in refs for t in range(trials)]

    def work(job):
        ref, t = job
        r = graded_run(pb, dirs[ref], profile, model)
        return {"task": ref.split("/")[-1], "trial": t, **r}

    results = []
    with ThreadPoolExecutor(max_workers=max(1, parallel)) as ex:
        for fut in as_completed([ex.submit(work, j) for j in jobs]):
            r = fut.result()
            results.append(r)
            tag = "ERR" if r["error"] else f"{r['score']:.3f}"
            print(f"  [{label}] {r['task']} → {tag}", flush=True)
    failures = [{"task": r["task"], "feedback": r["feedback"]}
                for r in results if capture_failures and not r["passed"]]
    m = mean(r["score"] for r in results) if results else 0.0
    return {"label": label, "mean": round(m, 3), "results": results, "failures": failures}


def bookmarks(pb: Pillbox, set_name: str, split: str) -> list[str]:
    d = pb._json(["--pillbox", pb.cfg.evals_pillbox, "bookmark", "list", "--json"], timeout=30)
    return sorted(b["name"] for b in d["bookmarks"] if b["name"].startswith(f"{set_name}/{split}/"))


def run_gate(cfg: Config, run_id: str, ts: str) -> dict:
    pb = Pillbox(cfg)
    train = bookmarks(pb, cfg.task_set, "train")
    held = bookmarks(pb, cfg.task_set, "held-out")
    if not train or not held:
        raise PillboxError(f"need frozen {cfg.task_set}/{{train,held-out}}/* in '{cfg.evals_pillbox}'")
    if cfg.limit:  # fast-iteration tier: cap tasks per split
        train, held = train[:cfg.limit], held[:cfg.limit]
    print(f"frozen '{cfg.task_set}': train={len(train)} held={len(held)} | worker={cfg.worker_model} "
          f"reflector={cfg.reflector_model} trials={cfg.trials} parallel={cfg.parallel}")
    playbook = open(cfg.playbook).read() if cfg.playbook and os.path.exists(cfg.playbook) else ""
    P = cfg.parallel

    with tempfile.TemporaryDirectory() as tmp:
        print("== baseline (held) ==")
        baseline = eval_arm(pb, "base", held, "", cfg.worker_model, tmp, cfg.trials, P)
        print("== train (capture failures for the reflector) ==")
        tr = eval_arm(pb, "train", train, "", cfg.worker_model, tmp, cfg.trials, P, capture_failures=True)
        print(f"== distill profile (reflector={cfg.reflector_model}) from {len(tr['failures'])} failures ==")
        profile = distill(pb, tr["failures"], cfg.reflector_model)
        gepa = None
        if profile.strip():
            print("== GEPA (held) ==")
            gepa = eval_arm(pb, "gepa", held, profile, cfg.worker_model, tmp, cfg.trials, P)
        else:
            # CRITICAL: an empty profile means the GEPA arm would run the WORKER with
            # nothing prepended — identical to baseline — and any "lift" would be pure
            # run-to-run noise reported as a real result. Skip it loudly; never fabricate.
            print("== GEPA: SKIPPED — distill produced an EMPTY profile (reflector wrote none). "
                  "Refusing to run a profile-less arm and mislabel the noise as a lift. ==")
        ace = None
        if playbook:
            # NOTE: static-playbook proxy, NOT real ACE runtime context evolution.
            # A null result here ≠ "ACE doesn't help" (see module docstring).
            print("== ACE / playbook (held) [STATIC-PLAYBOOK PROXY — not real ACE] ==")
            ace = eval_arm(pb, "ace", held, playbook, cfg.worker_model, tmp, cfg.trials, P)

    return {
        "run_id": run_id, "timestamp": ts, "config": asdict(cfg),
        "profile": profile,
        "distill_ok": bool(profile.strip()),
        "arms": {
            "baseline": baseline,
            **({"gepa": gepa} if gepa else {}),
            **({"ace": ace} if ace else {}),
        },
        "lift": {
            **({"gepa_over_baseline": round(gepa["mean"] - baseline["mean"], 3)} if gepa else {}),
            **({"ace_over_baseline": round(ace["mean"] - baseline["mean"], 3)} if ace else {}),
        },
    }


def report(artifact: dict):
    a = artifact["arms"]
    print(f"\n=== gate {artifact['run_id']} — held-out mean rubric score "
          f"(worker={artifact['config']['worker_model']}, trials={artifact['config']['trials']}) ===")
    for k, arm in a.items():
        print(f"  {k:9s}: {arm['mean']:.3f}")
    if "gepa_over_baseline" in artifact["lift"]:
        lift = artifact["lift"]["gepa_over_baseline"]
        print(f"lift (GEPA − baseline): {lift:+.3f}  → "
              f"{'GEPA helps' if lift > 0.05 else 'no meaningful lift (noise)'}")
    else:
        print("GEPA arm: SKIPPED (distill produced no profile) — no lift to report.")


def main():
    ap = argparse.ArgumentParser(description="optimization-gate eval (worker/reflector split)")
    ap.add_argument("--pillbox", default=os.environ.get("PILLBOX", "/tmp/pillbox-lk"))
    ap.add_argument("--worker-model", required=True)
    ap.add_argument("--reflector-model", required=True)
    ap.add_argument("--task-set", default="aider")
    ap.add_argument("--evals-pillbox", default="evals")
    ap.add_argument("--trials", type=int, default=1)
    ap.add_argument("--max-wait", type=int, default=240)
    ap.add_argument("--parallel", type=int, default=1, help="concurrent worker VMs (>1 only for LOCAL models; hosted plans throttle)")
    ap.add_argument("--limit", type=int, default=0, help="cap tasks per split (0=all; fast iteration)")
    ap.add_argument("--in-sandbox", action="store_true")
    ap.add_argument("--runner-image", default=os.environ.get("PILLBOX_RUNNER_IMAGE", "pillbox-runner:l7"))
    ap.add_argument("--playbook", default="")
    ap.add_argument("--out", default="gate-run.json")
    ap.add_argument("--run-id", default="gate")
    ap.add_argument("--timestamp", default="", help="ISO stamp for the artifact (caller-supplied)")
    args = ap.parse_args()
    # Line-buffer stdout so progress is observable when redirected to a file/pipe
    # (Python block-buffers a non-TTY by default — otherwise a long run looks dead).
    try:
        sys.stdout.reconfigure(line_buffering=True)
    except Exception:
        pass
    cfg = Config(
        pillbox=args.pillbox, worker_model=args.worker_model, reflector_model=args.reflector_model,
        task_set=args.task_set, evals_pillbox=args.evals_pillbox, trials=args.trials,
        max_wait=args.max_wait, in_sandbox=args.in_sandbox, runner_image=args.runner_image,
        playbook=args.playbook, out=args.out, parallel=args.parallel, limit=args.limit,
    )
    artifact = run_gate(cfg, args.run_id, args.timestamp)
    with open(cfg.out, "w") as f:
        json.dump(artifact, f, indent=2)
    print(f"\nartifact → {cfg.out}")
    report(artifact)


if __name__ == "__main__":
    main()
