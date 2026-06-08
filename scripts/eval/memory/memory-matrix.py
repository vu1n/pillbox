#!/usr/bin/env python3
"""memory-matrix.py — measure whether kypp memory actually changes agent behavior.

This is the experiment the optimization gate couldn't be: a planted, out-of-band
answer makes "did memory help" near-binary, so it sidesteps the variance wall that
killed lift measurement. For each validity lever (recency/authority/corroboration/
scope/pitfall) we run the SAME task under three arms and compare:

  off        — no memory injected. The answer is out-of-band, so this is the floor:
               app_rate here should be ~0. If it isn't, the task leaks (not a memory
               result) — surfaced loudly, because it invalidates the lever.
  on         — the lever's memory seeded, brief prepended. app_rate here is the recall
               effect.  lift = on − off.
  distractor — `on` PLUS irrelevant noise claims in the brief. Tests context pollution:
               distractor < on means noise is crowding out the signal.

For the `scope` lever the metric flips: there's nothing to apply, the test is that a
DIFFERENT project's fact does NOT leak in — measured as leak_rate (lower is better).

We inject the brief OURSELVES (kypp briefing → prompt prefix) rather than via
`pillbox run --memory`, to isolate the memory-validity variable from pillbox's brief
plumbing (the single-positional heuristic, project derivation). Same payload, fewer
confounds.

  python3 memory-matrix.py --dry-run            # seed+brief+compose, NO agent (plumbing check)
  python3 memory-matrix.py --trials 3 --out mem-run.json
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import tempfile
from contextlib import contextmanager
from dataclasses import dataclass

from _kypp import briefing, kypp_python, seed

HERE = os.path.dirname(os.path.abspath(__file__))

# Irrelevant accepted facts for the distractor arm — plausible project lore that
# pads the brief without touching any lever's answer.
DISTRACTORS = [
    ("logging format", "the project logs in structured JSON to stdout"),
    ("ci runner", "CI runs on ubuntu-latest with the stable toolchain"),
    ("license", "the project is MIT licensed"),
    ("formatter", "code is formatted with the repo's standard formatter on commit"),
    ("changelog", "user-facing changes get a CHANGELOG entry"),
]


@dataclass
class Cfg:
    pillbox: str
    model: str
    runner_image: str
    max_wait: int = 240
    trials: int = 1
    arms: tuple[str, ...] = ("off", "on", "distractor")


def compose(brief: str, prompt: str) -> str:
    if not brief:
        return prompt
    return f"## Project memory (kypp)\n{brief}\n\n## Task\n{prompt}"


class Pillbox:
    """Slim libkrun-opencode driver (mirrors gate.py): start → drive → score → rm,
    per-id teardown even on error."""

    def __init__(self, cfg: Cfg):
        self.cfg = cfg
        self.env = {**os.environ, "PILLBOX_BACKEND": "libkrun",
                    "PILLBOX_RUNNER_IMAGE": cfg.runner_image}

    def _json(self, args, timeout):
        p = subprocess.run([self.cfg.pillbox, *args], capture_output=True, text=True,
                           env=self.env, timeout=timeout)
        if p.returncode != 0:
            # Surface the real CLI error — a non-zero `run`/`info` yields empty stdout,
            # so a bare json.loads would crash with an opaque JSONDecodeError instead.
            raise RuntimeError(f"pillbox {' '.join(args[:2])} failed (exit {p.returncode}): {p.stderr.strip()[:300]}")
        return json.loads(p.stdout)

    @contextmanager
    def session(self, workspace: str):
        sid = None
        try:
            d = self._json(["run", "--agent", "opencode", "--json", "--workspace", workspace,
                            "--model", self.cfg.model], timeout=120)
            sid = d["session"]["id"]
            clone = self._json(["session", "info", sid, "--json"], timeout=30)["session"].get("workspace", "")
            if not clone:
                raise RuntimeError(f"session {sid}: no result-workspace (backend not libkrun?)")
            yield sid, clone
        finally:
            if sid:
                subprocess.run([self.cfg.pillbox, "session", "rm", sid],
                               capture_output=True, env=self.env, timeout=60)

    def drive(self, sid: str, prompt: str):
        # A failed `send` (dead session, transport hiccup) must NOT be scored as a real
        # negative — it would inflate the off-arm floor / poison a routing verdict. Fail
        # loud; the caller records it as an errored cell, not applied=False.
        s = subprocess.run([self.cfg.pillbox, "session", "send", sid, prompt],
                           capture_output=True, text=True, env=self.env, timeout=60)
        if s.returncode != 0:
            raise RuntimeError(f"session send failed (exit {s.returncode}): {s.stderr.strip()[:300]}")
        try:
            subprocess.run([self.cfg.pillbox, "session", "wait-idle", sid, "--timeout",
                            str(self.cfg.max_wait)], capture_output=True, env=self.env,
                           timeout=self.cfg.max_wait + 60)
        except subprocess.TimeoutExpired:
            pass  # turn ran long; grade whatever landed (documented tolerance)

    def score_cmd(self, sid: str, clone: str) -> dict:
        return self._json(["session", "score", sid, "--workspace", clone, "--cmd",
                           "sh grade.sh", "--json"], timeout=self.cfg.max_wait + 60)


def load_tasks(tasks_dir: str) -> list[dict]:
    tasks = []
    for lever in sorted(os.listdir(tasks_dir)):
        d = os.path.join(tasks_dir, lever)
        meta_path = os.path.join(d, "expected.json")
        if not os.path.isfile(meta_path):
            continue
        meta = json.load(open(meta_path))
        meta["dir"] = d
        meta["prompt"] = open(os.path.join(d, "prompt.txt")).read().strip()
        tasks.append(meta)
    return tasks


def arm_ops(task: dict, arm: str) -> list:
    """The seed ops for an arm. off → nothing; on → the lever's ops; distractor →
    on plus irrelevant noise in the BRIEF project."""
    if arm == "off":
        return []
    ops = list(task["seed"])
    if arm == "distractor":
        bp = task["brief_project"]
        ops += [{"op": "claim", "type": "fact", "subject": s, "content": c,
                 "accept": True, "project": bp} for s, c in DISTRACTORS]
    return ops


def run_cell(pb: Pillbox, py: str, task: dict, arm: str) -> dict:
    """One (lever, arm) run. `applied` = the hidden grader passed, which is lever-
    specific: for lift levers it means the memory value was correctly applied; for
    `scope` it means NO cross-project leak. Higher app_rate is always the good outcome."""
    bp = task["brief_project"]
    with tempfile.TemporaryDirectory() as tmp:
        db = os.path.join(tmp, "k.db")
        ops = arm_ops(task, arm)
        if ops:
            seed(py, db, bp, ops)
        brief = briefing(db, bp) if arm != "off" else ""
        prompt = compose(brief, task["prompt"])
        ws = os.path.join(tmp, "ws")
        shutil.copytree(os.path.join(task["dir"], "workspace"), ws)
        try:
            with pb.session(ws) as (sid, clone):
                pb.drive(sid, prompt)
                shutil.copytree(os.path.join(task["dir"], "grader"), clone, dirs_exist_ok=True)
                v = pb.score_cmd(sid, clone)
            return {"applied": bool(v.get("passed")), "score": float(v.get("score", 0.0)),
                    "brief_len": len(brief), "error": None}
        except (RuntimeError, subprocess.TimeoutExpired, json.JSONDecodeError) as e:
            return {"applied": False, "score": 0.0, "brief_len": len(brief), "error": str(e)}


def dry_run(py: str, tasks: list[dict], arms: tuple[str, ...]):
    """No agent: seed each arm, build the brief, print the composed prompt. Confirms
    the seed→brief→prompt plumbing end-to-end before spending VMs. Honors --arms so
    the dry-run exercises exactly the arms a real run would."""
    for task in tasks:
        print(f"\n===== {task['lever']} (metric={task['metric']}, brief_project={task['brief_project']}) =====")
        for arm in arms:
            with tempfile.TemporaryDirectory() as tmp:
                db = os.path.join(tmp, "k.db")
                ops = arm_ops(task, arm)
                if ops:
                    seed(py, db, task["brief_project"], ops)
                brief = briefing(db, task["brief_project"]) if arm != "off" else ""
                print(f"\n--- arm={arm} (brief {len(brief)} chars) ---")
                print(compose(brief, task["prompt"])[:600])


def report(artifact: dict):
    print("\n=== memory matrix — application rate by lever × arm ===")
    print(f"(model={artifact['config']['model']}, trials={artifact['config']['trials']})\n")
    hdr = f"  {'lever':14s} {'metric':10s} " + " ".join(f"{a:>11s}" for a in artifact["config"]["arms"])
    print(hdr)
    for lever, row in artifact["levers"].items():
        cells = " ".join(f"{row['arms'].get(a, {}).get('app_rate', float('nan')):>11.3f}" for a in artifact["config"]["arms"])
        print(f"  {lever:14s} {row['metric']:10s} {cells}")
    print()
    for lever, row in artifact["levers"].items():
        if row["metric"] == "lift":
            # A verdict needs both off (the floor) and on (the effect) actually run;
            # a missing arm is NOT a 0.0 rate — skip rather than fabricate.
            if "off" not in row["arms"] or "on" not in row["arms"]:
                print(f"  {lever:14s} (need both off+on arms for a lift verdict — skipped)")
                continue
            off = row["arms"]["off"]["app_rate"]
            on = row["arms"]["on"]["app_rate"]
            verdict = ("MEMORY WORKS" if (on - off) > 0.5 and off < 0.25 else
                       "TASK LEAKS (off too high)" if off >= 0.25 else
                       "weak/no effect")
            print(f"  {lever:14s} lift(on−off)={on - off:+.3f}  → {verdict}")
        else:  # scope: the grader passes when there's NO leak, so app_rate is the
               # scope-HELD rate; the leak rate is its complement.
            if "on" not in row["arms"]:
                print(f"  {lever:14s} (need the on arm for a scope verdict — skipped)")
                continue
            leak = round(1 - row["arms"]["on"]["app_rate"], 3)
            print(f"  {lever:14s} leak_rate(on)={leak:.3f}  → {'SCOPE HOLDS' if leak < 0.25 else 'CROSS-PROJECT LEAK'}")


def main():
    ap = argparse.ArgumentParser(description="memory-validity matrix (lever × arm)")
    ap.add_argument("--pillbox", default=os.environ.get("PILLBOX", "./target/debug/pillbox"))
    ap.add_argument("--model", default=os.environ.get("MODEL", "zai-coding-plan/glm-4.5-air"))
    ap.add_argument("--runner-image", default=os.environ.get("PILLBOX_RUNNER_IMAGE", "pillbox-runner:l7"))
    ap.add_argument("--tasks", default=os.path.join(HERE, "tasks"))
    ap.add_argument("--levers", default="", help="comma list to subset (default: all)")
    ap.add_argument("--arms", default="off,on,distractor")
    ap.add_argument("--trials", type=int, default=1)
    ap.add_argument("--max-wait", type=int, default=240)
    ap.add_argument("--out", default="mem-run.json")
    ap.add_argument("--dry-run", action="store_true", help="seed+brief+compose only, no agent")
    args = ap.parse_args()

    py = kypp_python()
    tasks = load_tasks(args.tasks)
    if args.levers:
        want = set(args.levers.split(","))
        tasks = [t for t in tasks if t["lever"] in want]
    if not tasks:
        raise SystemExit(f"no tasks under {args.tasks} (run gen-memory-tasks.py first)")

    arms = tuple(args.arms.split(","))
    if args.dry_run:
        dry_run(py, tasks, arms)
        return

    cfg = Cfg(pillbox=args.pillbox, model=args.model, runner_image=args.runner_image,
              max_wait=args.max_wait, trials=args.trials, arms=arms)
    pb = Pillbox(cfg)

    levers = {}
    for task in tasks:
        lever = task["lever"]
        levers[lever] = {"metric": task["metric"], "arms": {}}
        for arm in arms:
            runs = []
            for t in range(cfg.trials):
                r = run_cell(pb, py, task, arm)
                runs.append(r)
                tag = "ERR" if r["error"] else ("applied" if r["applied"] else "no")
                print(f"  [{lever}/{arm}] trial {t} → {tag}", flush=True)
            applied = [r["applied"] for r in runs]
            levers[lever]["arms"][arm] = {
                "app_rate": round(sum(applied) / len(applied), 3) if applied else 0.0,
                "runs": runs,
            }

    artifact = {"config": {"model": cfg.model, "trials": cfg.trials, "arms": list(arms)},
                "levers": levers}
    with open(args.out, "w") as f:
        json.dump(artifact, f, indent=2)
    print(f"\nartifact → {args.out}")
    report(artifact)


if __name__ == "__main__":
    main()
