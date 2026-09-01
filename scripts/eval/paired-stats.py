#!/usr/bin/env python3
"""Paired-by-task statistics for the sensitivity check (the gate-before-the-gate).

The optimization-gate's failure mode was comparing *means of independent runs* —
the per-task difficulty term dominates the variance and swamps any lift. This tool
does the paired analysis the eval design (docs/optimization-eval-family.md §4)
calls for:

  - per-task paired difference d(t) = mean_score(B,t) - mean_score(A,t), so the
    per-task difficulty term differences out;
  - a bootstrap CI over *tasks* (the replication unit) on mean d;
  - the empirical within-(task,condition) SD σ̂ — the number that decides whether
    the rig is sensitive enough (target ≈ 0.10).

Input: JSONL on stdin (or a path arg), one record per trial:
  {"task": "...", "cond": "A"|"B", "trial": 0, "score": 0.0..1.0, "cost": 0.0}
`cond` values: the FIRST distinct cond seen is the baseline (A), the second is the
treatment (B) — or pass --baseline NAME --treatment NAME explicitly.

Output: a JSON verdict on stdout:
  {sigma_hat, mean_d, ci_low, ci_high, n_tasks, n_trials_per_cell, sensitive}
`sensitive` is true iff σ̂ ≤ --sigma-target AND the lift CI excludes 0 (ci_low>0)
— i.e. the rig can both keep variance low AND detect the (planted) lift.

Deterministic: the bootstrap is seeded (--seed, default 0) so a re-run reproduces
the CI exactly — the eval design's "durable, resumable" requirement.

Self-test: `paired-stats.py --self-test` generates synthetic data with a KNOWN
injected lift and KNOWN σ and asserts the tool recovers them — the gate on the
gate's own gate. No pillbox/runtime needed.
"""
import argparse
import json
import statistics
import sys


def load_records(stream):
    recs = []
    for line in stream:
        line = line.strip()
        if not line:
            continue
        recs.append(json.loads(line))
    return recs


def _mean(xs):
    return sum(xs) / len(xs) if xs else 0.0


def analyze(records, baseline=None, treatment=None, sigma_target=0.10, seed=0, n_boot=10000):
    # cells[(task, cond)] = [score, score, ...]
    cells = {}
    conds_in_order = []
    for r in records:
        task, cond = r["task"], r["cond"]
        if cond not in conds_in_order:
            conds_in_order.append(cond)
        cells.setdefault((task, cond), []).append(float(r["score"]))

    if baseline is None or treatment is None:
        if len(conds_in_order) < 2:
            raise SystemExit(f"need 2 conditions, saw {conds_in_order}")
        baseline = baseline or conds_in_order[0]
        treatment = treatment or conds_in_order[1]

    # σ̂: pooled within-(task,cond) SD — the per-cell trial-to-trial spread. Cells
    # with <2 trials contribute no variance estimate (skipped, not zero).
    cell_sds = [statistics.stdev(v) for v in cells.values() if len(v) >= 2]
    sigma_hat = _mean(cell_sds)

    # Paired per-task differences over tasks present in BOTH conditions.
    tasks = sorted({t for (t, c) in cells})
    diffs = []
    for t in tasks:
        a, b = cells.get((t, baseline)), cells.get((t, treatment))
        if a and b:
            diffs.append(_mean(b) - _mean(a))
    if not diffs:
        raise SystemExit("no task appears in both conditions — nothing to pair")

    mean_d = _mean(diffs)

    # Bootstrap CI over tasks (resample the diffs with replacement). Seeded LCG so
    # the result is reproducible without importing a heavyweight RNG.
    state = (seed * 2862933555777941757 + 3037000493) & ((1 << 64) - 1)

    def rand_idx(n):
        nonlocal state
        state = (state * 6364136223846793005 + 1442695040888963407) & ((1 << 64) - 1)
        return (state >> 33) % n

    n = len(diffs)
    boot_means = []
    for _ in range(n_boot):
        s = sum(diffs[rand_idx(n)] for _ in range(n))
        boot_means.append(s / n)
    boot_means.sort()
    ci_low = boot_means[int(0.025 * n_boot)]
    ci_high = boot_means[int(0.975 * n_boot)]

    trials = [len(v) for v in cells.values()]
    return {
        "baseline": baseline,
        "treatment": treatment,
        "n_tasks": len(diffs),
        "n_trials_per_cell": min(trials),
        "sigma_hat": round(sigma_hat, 4),
        "mean_d": round(mean_d, 4),
        "ci_low": round(ci_low, 4),
        "ci_high": round(ci_high, 4),
        # The rig is usable iff variance is low AND the (planted) lift is visible.
        "sensitive": bool(sigma_hat <= sigma_target and ci_low > 0.0),
        "sigma_target": sigma_target,
    }


def _self_test():
    """Generate synthetic trials with a known lift (0.10) and known per-cell σ
    (~0.08), confirm the tool recovers both and calls it sensitive — then a
    high-σ variant it must call NOT sensitive."""
    import random

    def synth(lift, sigma, n_tasks=12, trials=3, seed=1):
        rng = random.Random(seed)
        out = []
        for ti in range(n_tasks):
            base = rng.uniform(0.3, 0.7)  # per-task difficulty (differenced out by pairing)
            for cond, mu in (("A", base), ("B", base + lift)):
                for tr in range(trials):
                    s = min(1.0, max(0.0, rng.gauss(mu, sigma)))
                    out.append({"task": f"t{ti}", "cond": cond, "trial": tr, "score": s, "cost": 0.01})
        return out

    lo = analyze(synth(lift=0.10, sigma=0.08))
    assert lo["sigma_hat"] <= 0.12, lo
    assert lo["ci_low"] > 0, lo  # the planted lift is detected
    assert lo["sensitive"], lo
    assert abs(lo["mean_d"] - 0.10) < 0.05, lo  # recovers the injected lift

    hi = analyze(synth(lift=0.10, sigma=0.30))
    assert hi["sigma_hat"] > 0.12, hi
    assert not hi["sensitive"], hi  # variance too high → rig not trusted

    # A true-null family (no lift): the CI must straddle 0 → not "sensitive".
    nul = analyze(synth(lift=0.0, sigma=0.08))
    assert nul["ci_low"] <= 0 <= nul["ci_high"], nul
    assert not nul["sensitive"], nul
    print("self-test ok:", json.dumps({"low_sigma": lo, "high_sigma": hi, "null": nul}, indent=2))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("path", nargs="?", help="JSONL records file (default: stdin)")
    ap.add_argument("--baseline", help="baseline cond name (default: first seen)")
    ap.add_argument("--treatment", help="treatment cond name (default: second seen)")
    ap.add_argument("--sigma-target", type=float, default=0.10)
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args()

    if args.self_test:
        _self_test()
        return

    stream = open(args.path) if args.path else sys.stdin
    records = load_records(stream)
    verdict = analyze(
        records,
        baseline=args.baseline,
        treatment=args.treatment,
        sigma_target=args.sigma_target,
        seed=args.seed,
    )
    print(json.dumps(verdict, indent=2))


if __name__ == "__main__":
    main()
