#!/usr/bin/env python3
"""Generate the synthetic task family for the sensitivity check (the
gate-before-the-gate; docs/optimization-eval-family.md §5).

Design — a STRUCTURALLY-GUARANTEED, uniformly-planted lift:
  - Each task implements a different small pure function (varied real work, so the
    family isn't one task repeated).
  - EVERY prompt deliberately OMITS the same arbitrary contract: "return -1 on
    empty input." The hidden tests check it; the prompt never mentions it.
  - The baseline can't reliably guess an arbitrary sentinel (it raises, or returns
    0/None/[]), so it fails that one criterion; the happy-path criteria it passes.
  - The single shared oracle.md states the contract — so arm B (prompt + oracle)
    passes it. The lift is therefore the contract-criterion's weight, IDENTICAL
    across tasks and structurally positive: exactly the "known planted lift a
    perfect router/memory would surface" the §5 check needs, and it works with
    sensitivity-check.sh's single-profile arm-B (the hint is uniform).
  - Rubric decomposition (N happy criteria + 1 contract criterion) gives partial
    credit → a score band + the variance-of-a-mean-of-Bernoullis reduction.

This is Option-C synthetic: legitimate for testing whether the RIG can detect a
lift, NOT for claiming optimizer transfer (the verdict's warning). The real
bakeoff needs Option-A in-repo microtasks.

Usage: gen-sensitivity-tasks.py [OUTDIR]   (default: scripts/eval/sensitivity-tasks)
Writes OUTDIR/oracle.md + OUTDIR/<id>/{prompt.txt,workspace/solution.py,grader/rubric.txt}.
Then: TRIALS=3 sensitivity-check.sh OUTDIR/oracle.md OUTDIR/t01 OUTDIR/t02 ...
"""
import os
import sys

# (fn_name, one-line spec for the prompt, [(args_repr, expected_repr), ...] happy cases)
# Keep happy cases unambiguous and the impl small; the empty case is the planted,
# prompt-omitted contract (added uniformly below). Args are list literals.
FUNCS = [
    ("sum_all", "return the sum of the numbers in the list `xs`",
     [("[1, 2, 3]", "6"), ("[10]", "10"), ("[-2, 2, 5]", "5")]),
    ("product", "return the product of the numbers in the list `xs`",
     [("[2, 3, 4]", "24"), ("[5]", "5"), ("[-1, 6]", "-6")]),
    ("count_evens", "return how many numbers in `xs` are even",
     [("[1, 2, 3, 4]", "2"), ("[2, 4, 6]", "3"), ("[1, 3]", "0")]),
    ("range_span", "return the difference between the largest and smallest number in `xs`",
     [("[1, 5, 3]", "4"), ("[7]", "0"), ("[-2, 2]", "4")]),
    ("count_positives", "return how many numbers in `xs` are strictly greater than zero",
     [("[-1, 2, 3]", "2"), ("[0, 0, 1]", "1"), ("[5, 6]", "2")]),
    ("second_largest", "return the second-largest DISTINCT value in `xs` (assume xs has ≥2 distinct values when non-empty)",
     [("[3, 1, 2]", "2"), ("[5, 5, 3]", "3"), ("[9, 1, 9, 4]", "4")]),
    ("abs_max", "return the value in `xs` with the largest absolute value (return the value itself, keeping its sign)",
     [("[1, -7, 3]", "-7"), ("[2, 2]", "2"), ("[-1, -1, 4]", "4")]),
    ("alternating_sum", "return xs[0] - xs[1] + xs[2] - ... (alternating signs from the left)",
     [("[1, 2, 3]", "2"), ("[10, 1]", "9"), ("[5]", "5")]),
    ("count_runs", "return the number of maximal runs of equal adjacent values in `xs`",
     [("[1, 1, 2, 2, 2, 3]", "3"), ("[4, 4, 4]", "1"), ("[1, 2, 1]", "3")]),
    ("digit_sum_max", "return the largest digit-sum among the non-negative integers in `xs` (e.g. 91 → 10)",
     [("[12, 91, 5]", "10"), ("[100, 7]", "7"), ("[19]", "10")]),
    ("nth_smallest", "return the 2nd-smallest value in `xs` (assume xs has ≥2 values when non-empty; duplicates count)",
     [("[3, 1, 2]", "2"), ("[5, 5, 9]", "5"), ("[8, 2, 4, 2]", "2")]),
    ("balanced_diff", "return the absolute difference between the sum of even-indexed and odd-indexed elements of `xs`",
     [("[1, 2, 3]", "2"), ("[10, 4]", "6"), ("[5]", "5")]),
]

EMPTY_SENTINEL = "-1"

# Reference CORRECT bodies (happy path), in the uniform form
# `return -1 if not xs else <happy>` so the self-test can derive a BASELINE
# (happy only — raises/wrong on empty) by stripping the guard. These double as
# the canonical intended behavior and certify every rubric's happy expected
# values (a wrong expected would make a task ungradeable even for a correct agent).
REF_HAPPY = {
    "sum_all": "sum(xs)",
    "product": "__import__('math').prod(xs)",
    "count_evens": "sum(1 for x in xs if x % 2 == 0)",
    "range_span": "max(xs) - min(xs)",
    "count_positives": "sum(1 for x in xs if x > 0)",
    "second_largest": "sorted(set(xs))[-2]",
    "abs_max": "max(xs, key=abs)",
    "alternating_sum": "sum(x * (-1) ** i for i, x in enumerate(xs))",
    "count_runs": "1 + sum(1 for i in range(1, len(xs)) if xs[i] != xs[i - 1])",
    "digit_sum_max": "max(sum(int(d) for d in str(x)) for x in xs if x >= 0)",
    "nth_smallest": "sorted(xs)[1]",
    "balanced_diff": "abs(sum(xs[0::2]) - sum(xs[1::2]))",
}

ORACLE = """\
CONTRACT (applies to the function you implement):

- When the input list is **empty**, the function MUST return the integer `-1`.
  Do not raise, and do not return `0`, `None`, or `[]` — return exactly `-1`.

Implement the requested behavior for non-empty inputs as described, AND honor this
empty-input contract.
"""

STUB = '''\
def {name}(xs):
    """{spec}."""
    raise NotImplementedError
'''

PROMPT = """\
Implement `{name}(xs)` in `solution.py`: {spec}.

`xs` is a list of integers. Edit ONLY `solution.py`. Keep the function name and
signature exactly as given.
"""


def _crit(name, call, expected):
    # One rubric criterion: import the fn and assert. Single-quoted `-c`, so the
    # python body uses double quotes only (path ".") — no quote collision.
    body = (
        'import sys;sys.path.insert(0,".");'
        f'from solution import {name} as f;'
        f'assert f({call})=={expected}'
    )
    return f'{name} {call}={expected} :: python3 -c \'{body}\''


def write_task(outdir, idx, name, spec, cases):
    tid = f"t{idx:02d}"
    base = os.path.join(outdir, tid)
    os.makedirs(os.path.join(base, "workspace"), exist_ok=True)
    os.makedirs(os.path.join(base, "grader"), exist_ok=True)
    with open(os.path.join(base, "workspace", "solution.py"), "w") as f:
        f.write(STUB.format(name=name, spec=spec))
    with open(os.path.join(base, "prompt.txt"), "w") as f:
        f.write(PROMPT.format(name=name, spec=spec))
    lines = [_crit(name, call, exp) for call, exp in cases]
    # The planted, prompt-omitted contract criterion (the lift the oracle supplies).
    lines.append(_crit(name, "[]", EMPTY_SENTINEL))
    with open(os.path.join(base, "grader", "rubric.txt"), "w") as f:
        f.write("\n".join(lines) + "\n")
    return tid


def _run_rubric(rubric_path, solution_src):
    """Run every criterion COMMAND in a temp dir holding `solution.py`; return
    (passes, total). Criteria are `NAME :: COMMAND` (COMMAND is `sh -c`-run)."""
    import subprocess
    import tempfile

    with tempfile.TemporaryDirectory() as d:
        with open(os.path.join(d, "solution.py"), "w") as f:
            f.write(solution_src)
        passes = total = 0
        for line in open(rubric_path):
            line = line.strip()
            if not line:
                continue
            cmd = line.split("::", 1)[1].strip()
            total += 1
            r = subprocess.run(cmd, shell=True, cwd=d, capture_output=True)
            passes += r.returncode == 0
        return passes, total


def _self_test():
    """Generate to a tempdir and certify the planted-lift structure WITHOUT an
    agent: a CORRECT solution passes every criterion; a BASELINE (happy logic, no
    empty handling) passes all but exactly one — the empty-contract criterion. So
    the oracle's lift is structurally one criterion per task. Catches any wrong
    happy expected value too (a correct ref would then fail its own rubric)."""
    import tempfile

    with tempfile.TemporaryDirectory() as outdir:
        with open(os.path.join(outdir, "oracle.md"), "w") as f:
            f.write(ORACLE)
        for i, (name, spec, cases) in enumerate(FUNCS):
            tid = write_task(outdir, i + 1, name, spec, cases)
            rubric = os.path.join(outdir, tid, "grader", "rubric.txt")
            happy = REF_HAPPY[name]
            correct = f"def {name}(xs):\n    return {EMPTY_SENTINEL} if not xs else {happy}\n"
            baseline = f"def {name}(xs):\n    return {happy}\n"
            cp, ct = _run_rubric(rubric, correct)
            bp, bt = _run_rubric(rubric, baseline)
            assert cp == ct, f"{tid} {name}: correct solution failed its rubric ({cp}/{ct}) — bad expected value"
            assert bp == bt - 1, f"{tid} {name}: baseline should fail exactly the empty criterion, got {bp}/{bt}"
        print(f"self-test ok: {len(FUNCS)} tasks; correct→all-pass, baseline→all-but-empty (lift = 1 criterion/task)")


def main():
    if "--self-test" in sys.argv[1:]:
        _self_test()
        return
    outdir = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "sensitivity-tasks")
    os.makedirs(outdir, exist_ok=True)
    with open(os.path.join(outdir, "oracle.md"), "w") as f:
        f.write(ORACLE)
    ids = [write_task(outdir, i + 1, n, s, c) for i, (n, s, c) in enumerate(FUNCS)]
    print(f"wrote {len(ids)} tasks + oracle.md to {outdir}")
    print("run: TRIALS=3 MODEL=<provider/model> scripts/eval/sensitivity-check.sh \\")
    print(f"       {outdir}/oracle.md " + " ".join(f"{outdir}/{t}" for t in ids))


if __name__ == "__main__":
    main()
