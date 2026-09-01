#!/usr/bin/env python3
"""cost-router.py — route a coding task to the CHEAPEST model that clears the bar,
and learn which model that is by remembering outcomes in kypp.

This is the measurable half of the routing story. "Which model is BETTER" is the
same fuzzy quality delta that drowned the optimization gate in variance — don't try
to learn it. "Did the cheaper model CLEAR THE BAR" is binary (the verifiable rubric/
cmd grade) and cost is observable (the §0 usage events). So the learnable policy is:
route to the cheapest model whose adequacy for this task-CLASS is corroborated, fall
back to exploring the next-cheapest, and record every outcome.

The routing policy IS memory. Each outcome is a kypp claim on subject
`route/<class>/<model>`; two independent passes (distinct session source_ids) get
consolidated to ACCEPTED — kypp's corroboration lever — at which point the router
treats the model as adequate-for-the-class and stops exploring. So the same validity
levers the memory matrix tests (corroboration, recency-supersession) govern routing:
a model that regresses can be `kypp correct`'d back to inadequate. pillbox exposes
the signals; kypp accrues the policy; this reads it. No optimizer inside pillbox.

  # one task; explores cheapest→up, records outcomes, prints the chosen model
  python3 cost-router.py --class py-bugfix --task-dir ../eval/memory/tasks/pitfall \
                         --ladder zai-coding-plan/glm-4.5-air,zai-coding-plan/glm-5.1

  python3 cost-router.py --class py-bugfix --explain   # show learned adequacy, no run
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import tempfile
from contextlib import contextmanager

HERE = os.path.dirname(os.path.abspath(__file__))
SEED_RUNNER = os.path.join(HERE, "..", "eval", "memory", "_seed_runner.py")


def kypp_python() -> str:
    kypp = shutil.which("kypp")
    if not kypp:
        raise SystemExit("`kypp` not on PATH")
    with open(kypp) as f:
        sb = f.readline().strip()
    return sb[2:].strip() if sb.startswith("#!") else "python3"


def record(py: str, project: str, ops: list) -> None:
    """Append routing-outcome claims to kypp via the shared seed runner. (Own copy:
    cost-router is in scripts/router/, so it can't same-dir-import scripts/eval/memory's
    _kypp; it reaches _seed_runner.py as a subprocess path instead — see SEED_RUNNER.)"""
    env = {**os.environ, "KYPP_PROJECT": project}
    p = subprocess.run([py, SEED_RUNNER, project], input=json.dumps(ops), text=True,
                       env=env, capture_output=True)
    if p.returncode != 0:
        # Surface the child's diagnostic — a swallowed outcome-record failure would
        # silently desync the learned routing policy from what actually happened.
        raise RuntimeError(f"_seed_runner failed (project={project}): {p.stderr.strip()}")


def recall_adequacy(klass: str, project: str) -> dict:
    """Read what memory knows about each model for this class. Returns
    {model: {"adequate": bool, "failed": bool}} from `route/<class>/<model>` claims:
    an ACCEPTED fact (corroborated ≥2 passes) → adequate; a pitfall → failed."""
    env = {**os.environ, "KYPP_PROJECT": project}
    # The route/<class>/ prefix filter below keeps this correct regardless of recall
    # ranking; the --limit 200 cap only bites if ONE project accumulates >200 route
    # claims AND a relevant one ranks below the cap — then an adequate model is missed
    # and re-explored (a cost regression, not a wrong answer). Raise it if that happens.
    p = subprocess.run(["kypp", "recall", klass, "--candidates", "--json",
                        "--project", project, "--limit", "200"],
                       env=env, capture_output=True, text=True)
    out: dict = {}
    try:
        claims = json.loads(p.stdout or "[]")
    except json.JSONDecodeError:
        return out
    prefix = f"route/{klass}/"
    for c in claims:
        subj = c.get("subject", "")
        if not subj.startswith(prefix):
            continue
        model = subj[len(prefix):]
        rec = out.setdefault(model, {"adequate": False, "failed": False})
        if c.get("type") == "fact" and c.get("status") == "accepted":
            rec["adequate"] = True
        if c.get("type") == "pitfall":
            rec["failed"] = True
    return out


def route_order(ladder: list[str], know: dict) -> list[str]:
    """Cheapest-first within each tier: corroborated-adequate, then unexplored, then
    known-failing last (still tried — a world-change may have fixed it)."""
    adequate = [m for m in ladder if know.get(m, {}).get("adequate")]
    failing = [m for m in ladder if know.get(m, {}).get("failed") and not know.get(m, {}).get("adequate")]
    untried = [m for m in ladder if m not in adequate and m not in failing]
    return adequate + untried + failing


class Pillbox:
    """Slim libkrun-opencode driver (a sibling of the ones in scripts/eval/gate.py and
    memory-matrix.py — kept per-tool by the repo's convention, not shared)."""

    def __init__(self, pillbox: str, runner_image: str, max_wait: int):
        self.pillbox = pillbox
        self.max_wait = max_wait
        self.env = {**os.environ, "PILLBOX_BACKEND": "libkrun",
                    "PILLBOX_RUNNER_IMAGE": runner_image}

    def _json(self, args, timeout):
        p = subprocess.run([self.pillbox, *args], capture_output=True, text=True,
                           env=self.env, timeout=timeout)
        if p.returncode != 0:
            # Surface the real CLI error. An infra failure (image missing, daemon down)
            # propagates and aborts the route loop loudly — it is NOT recorded as a
            # model verdict, so it can't poison the learned routing policy.
            raise RuntimeError(f"pillbox {' '.join(args[:2])} failed (exit {p.returncode}): {p.stderr.strip()[:300]}")
        return json.loads(p.stdout)

    @contextmanager
    def session(self, workspace: str, model: str):
        sid = None
        try:
            d = self._json(["run", "--agent", "opencode", "--json", "--workspace",
                            workspace, "--model", model], timeout=120)
            sid = d["session"]["id"]
            clone = self._json(["session", "info", sid, "--json"], timeout=30)["session"].get("workspace", "")
            if not clone:
                raise RuntimeError(f"session {sid}: no result-workspace (backend not libkrun?)")
            yield sid, clone
        finally:
            if sid:
                subprocess.run([self.pillbox, "session", "rm", sid],
                               capture_output=True, env=self.env, timeout=60)

    def drive(self, sid: str, prompt: str):
        # A failed `send` must NOT be scored as a model failure (it would record a
        # spurious pitfall and desync the policy). Fail loud → aborts this route.
        s = subprocess.run([self.pillbox, "session", "send", sid, prompt],
                           capture_output=True, text=True, env=self.env, timeout=60)
        if s.returncode != 0:
            raise RuntimeError(f"session send failed (exit {s.returncode}): {s.stderr.strip()[:300]}")
        try:
            subprocess.run([self.pillbox, "session", "wait-idle", sid, "--timeout",
                            str(self.max_wait)], capture_output=True, env=self.env,
                           timeout=self.max_wait + 60)
        except subprocess.TimeoutExpired:
            pass  # turn ran long; grade whatever landed (documented tolerance)

    def score(self, sid: str, clone: str, rubric: str | None) -> dict:
        args = ["session", "score", sid, "--workspace", clone, "--json"]
        args += ["--rubric", rubric] if rubric else ["--cmd", "sh grade.sh"]
        return self._json(args, timeout=self.max_wait + 60)

    def usd(self, sid: str) -> float:
        """Sum the session's §0 usage events → $ (same pricing env as lib.sh pb_usage)."""
        p = subprocess.run([self.pillbox, "session", "log", sid, "--type", "usage"],
                           capture_output=True, text=True, env=self.env, timeout=60)
        pin = float(os.environ.get("PRICE_IN_PER_M", "3.0"))
        pout = float(os.environ.get("PRICE_OUT_PER_M", "15.0"))
        pcr = float(os.environ.get("PRICE_CACHE_READ_PER_M", "0.30"))
        pcc = float(os.environ.get("PRICE_CACHE_CREATION_PER_M", "3.75"))
        i = o = cr = cc = 0
        for line in p.stdout.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                pl = json.loads(line)["payload"]
            except Exception:
                continue
            if pl.get("type") != "usage":
                continue
            i += pl.get("inputTokens") or 0
            o += pl.get("outputTokens") or 0
            cr += pl.get("cacheReadInputTokens") or 0
            cc += pl.get("cacheCreationInputTokens") or 0
        return round((i * pin + o * pout + cr * pcr + cc * pcc) / 1_000_000, 6)


def run_one(pb: Pillbox, task_dir: str, model: str) -> dict:
    """Run the task on `model`, grade against the hidden bar. → passed/score/usd/sid."""
    rubric = os.path.join(task_dir, "grader", "rubric.txt")
    use_rubric = os.path.exists(rubric)
    prompt = open(os.path.join(task_dir, "prompt.txt")).read()
    with tempfile.TemporaryDirectory() as tmp:
        ws = os.path.join(tmp, "ws")
        shutil.copytree(os.path.join(task_dir, "workspace"), ws)
        with pb.session(ws, model) as (sid, clone):
            pb.drive(sid, prompt)
            shutil.copytree(os.path.join(task_dir, "grader"), clone, dirs_exist_ok=True)
            v = pb.score(sid, clone, rubric if use_rubric else None)
            usd = pb.usd(sid)
        return {"model": model, "passed": bool(v.get("passed")),
                "score": float(v.get("score", 0.0)), "usd": usd, "sid": sid}


def outcome_ops(klass: str, project: str, model: str, runid: str, res: dict) -> list:
    subject = f"route/{klass}/{model}"
    if res["passed"]:
        # A pass is corroborating evidence; two independent passes consolidate to
        # accepted → the model becomes route-adequate for the class.
        return [
            {"op": "claim", "type": "fact", "subject": subject,
             "content": f"{model} cleared the bar for class {klass} (score {res['score']:.2f})",
             "source_ids": [runid], "project": project},
            {"op": "consolidate", "subject": subject, "accept_corroboration": 2},
        ]
    return [
        {"op": "claim", "type": "pitfall", "subject": subject,
         "content": f"{model} did NOT clear the bar for class {klass} (score {res['score']:.2f})",
         "source_ids": [runid], "project": project},
    ]


def main():
    ap = argparse.ArgumentParser(description="cost-router: cheapest model that clears the bar, learned via kypp")
    ap.add_argument("--class", dest="klass", required=True, help="task class (plain token, e.g. py-bugfix)")
    ap.add_argument("--task-dir", help="task dir (prompt.txt, workspace/, grader/)")
    ap.add_argument("--ladder", default=os.environ.get("ROUTER_LADDER", "zai-coding-plan/glm-4.5-air,zai-coding-plan/glm-5.1"),
                    help="models cheapest→most-capable, comma-separated")
    ap.add_argument("--project", default=os.environ.get("KYPP_PROJECT", "router"))
    ap.add_argument("--pillbox", default=os.environ.get("PILLBOX", "./target/debug/pillbox"))
    ap.add_argument("--runner-image", default=os.environ.get("PILLBOX_RUNNER_IMAGE", "pillbox-runner:dev"))
    ap.add_argument("--max-wait", type=int, default=240)
    ap.add_argument("--out", default="route-run.json")
    ap.add_argument("--explain", action="store_true", help="print learned adequacy + the route order, no run")
    args = ap.parse_args()

    ladder = [m.strip() for m in args.ladder.split(",") if m.strip()]
    py = kypp_python()
    know = recall_adequacy(args.klass, args.project)
    order = route_order(ladder, know)

    if args.explain:
        print(f"class={args.klass} project={args.project}")
        print(f"ladder (cheap→capable): {ladder}")
        for m in ladder:
            k = know.get(m, {})
            state = "ADEQUATE (corroborated)" if k.get("adequate") else "failing" if k.get("failed") else "unexplored"
            print(f"  {m:34s} {state}")
        print(f"route order: {order}")
        print(f"→ would try first: {order[0] if order else '(none)'}")
        return

    if not args.task_dir:
        raise SystemExit("--task-dir required (or use --explain)")

    pb = Pillbox(args.pillbox, args.runner_image, args.max_wait)
    print(f"class={args.klass}: route order {order}")
    attempts = []
    chosen = None
    for model in order:
        print(f"  → trying {model} ...", flush=True)
        res = run_one(pb, args.task_dir, model)
        attempts.append(res)
        record(py, args.project, outcome_ops(args.klass, args.project, model, res["sid"], res))
        tag = f"PASS score={res['score']:.2f} ${res['usd']:.4f}" if res["passed"] else f"fail score={res['score']:.2f}"
        print(f"    {model}: {tag}")
        if res["passed"]:
            chosen = model
            break

    artifact = {"class": args.klass, "project": args.project, "ladder": ladder,
                "route_order": order, "chosen": chosen,
                "total_usd": round(sum(a["usd"] for a in attempts), 6),
                "attempts": attempts}
    with open(args.out, "w") as f:
        json.dump(artifact, f, indent=2)
    print(f"\nartifact → {args.out}")
    print(f"chosen: {chosen or '(none cleared the bar)'} | "
          f"spent ${artifact['total_usd']:.4f} over {len(attempts)} attempt(s)")
    print("re-run after ≥2 passes on the cheap model: it consolidates to ADEQUATE → "
          "router skips exploration → cost drops. (`--explain` to watch the policy form.)")


if __name__ == "__main__":
    main()
