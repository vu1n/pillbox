#!/usr/bin/env python3
"""Generate the Option-A real-bug task set — the agentic-regime companion to the
synthetic sensitivity family (gen-sensitivity-tasks.py).

The §5 sensitivity check passed but on single-function synthetic tasks too clean
to exercise the variance that killed the prior real runs (agentic path divergence
over a real codebase). This builds the missing regime: **single-fault bugs
injected into REAL toolz functions, graded by toolz's REAL pytest suite.**
- Real codebase + real tests (Option-A's inject variant — docs/optimization-eval-family.md §2).
- Agentic: the agent gets the multi-module `toolz/` package + a symptom and must
  navigate to the faulty function and fix it (not fill a stub).
- Dep-light: toolz is pure-Python/zero-dep, so grading is fast + deterministic on
  the host (no pip-per-grade, no network) — graded via `uv run --with pytest`.
- Controllable single-fault: each bug is a unique find→replace caught by a named
  test, so validity + headroom are guaranteed (validated by --self-test, no agent).

Binary-ish by design: a single-fault bug breaks its regression test (SWE-bench is
binary the same way). That's the honest agentic grader; the σ̂ this family measures
is exactly the trial-to-trial Bernoulli variance the synthetic family lacked.

Usage: gen-toolz-tasks.py [OUTDIR] [--repo PATH]   (default OUTDIR: ./toolz-tasks)
  --repo : a toolz checkout (defaults to a cached clone at the pinned commit)
  --self-test : validate every bug WITHOUT generating (clean→tests pass, bugged→fail)

Then freeze + run: `freeze-split.sh <OUTDIR> toolz` → the sensitivity/bakeoff rig.
The generated workspaces are bulky (a toolz copy each) → generate on demand, don't commit.
"""
import argparse
import os
import shutil
import subprocess
import sys
import tempfile

REPO_URL = "https://github.com/pytoolz/toolz"
PIN = "568c2b8"  # pinned so the bug find→replace + test names stay valid
SRC = "toolz/itertoolz.py"
TESTS_DIR = "toolz/tests"

# Each bug: a UNIQUE find→replace in SRC (single fault), the toolz test(s) that
# catch it (the hidden grader), and a symptom prompt naming the broken function
# (realistic bug report) but not the fix.
BUGS = [
    dict(
        id="take_off_by_one",
        find="    return itertools.islice(seq, n)",
        replace="    return itertools.islice(seq, n + 1)",
        tests=["test_take"],
        prompt="`toolz.take(n, seq)` should yield the first n elements of seq, but it yields n+1.",
    ),
    dict(
        id="drop_off_by_one",
        find="    return itertools.islice(seq, n, None)",
        replace="    return itertools.islice(seq, n + 1, None)",
        tests=["test_drop"],
        prompt="`toolz.drop(n, seq)` should skip the first n elements, but it skips n+1.",
    ),
    dict(
        id="take_nth_offset",
        find="    return itertools.islice(seq, 0, None, n)",
        replace="    return itertools.islice(seq, 1, None, n)",
        tests=["test_take_nth"],
        prompt="`toolz.take_nth(n, seq)` should start at the first element (index 0), but it starts at the second.",
    ),
    dict(
        id="tail_slice",
        find="        return seq[-n:]",
        replace="        return seq[-n + 1:]",
        tests=["test_tail"],
        prompt="`toolz.tail(n, seq)` returns the wrong number of trailing elements for indexable sequences (one too few).",
    ),
    dict(
        id="sliding_window_drops_first",
        find="    return zip(*(collections.deque(itertools.islice(it, i), 0) or it",
        replace="    return zip(*(collections.deque(itertools.islice(it, i + 1), 0) or it",
        tests=["test_sliding_window"],
        prompt="`toolz.sliding_window(n, seq)` produces windows that are shifted/incorrect — the overlap is wrong.",
    ),
]

PROMPT_TMPL = """\
There is a bug in the `toolz` library (a pure-Python utility package). Symptom:

  {symptom}

Find the faulty function in the `toolz/` source and fix it. Edit only the source
under `toolz/`; keep the public function name and signature unchanged.
"""


def ensure_repo(repo_arg):
    if repo_arg:
        return repo_arg
    cache = os.path.join(tempfile.gettempdir(), "toolz-src")
    if not os.path.isdir(os.path.join(cache, ".git")):
        subprocess.run(["git", "clone", "--quiet", REPO_URL, cache], check=True)
    subprocess.run(["git", "-C", cache, "checkout", "--quiet", PIN], check=True)
    return cache


def _read(p):
    with open(p) as f:
        return f.read()


def _apply_bug(text, bug):
    if text.count(bug["find"]) != 1:
        raise SystemExit(f"{bug['id']}: find string not unique ({text.count(bug['find'])}x) — pin drift?")
    return text.replace(bug["find"], bug["replace"])


# Read-then-write: open("w") truncates, so the bugged text MUST be computed before
# the file is opened for writing (else _read sees an emptied file).
def _patch_file(path, bug):
    bugged = _apply_bug(_read(path), bug)
    with open(path, "w") as f:
        f.write(bugged)


def rubric_line(test):
    # Run from the clone root; toolz is importable from cwd. uv fetches pytest
    # ephemerally (host has no pytest; toolz is zero-dep so nothing else needed).
    cmd = f"uv run --no-project --with pytest python -m pytest {TESTS_DIR}/test_itertoolz.py::{test} -q"
    return f"{test} :: {cmd}"


def build_task(repo, outdir, bug):
    base = os.path.join(outdir, bug["id"])
    ws_pkg = os.path.join(base, "workspace", "toolz")
    # workspace = the toolz package MINUS tests (hidden), with the bug applied.
    shutil.copytree(os.path.join(repo, "toolz"), ws_pkg)
    shutil.rmtree(os.path.join(ws_pkg, "tests"), ignore_errors=True)
    _patch_file(os.path.join(ws_pkg, "itertoolz.py"), bug)
    # grader = the clean tests (injected at grade time) + the rubric.
    shutil.copytree(os.path.join(repo, TESTS_DIR), os.path.join(base, "grader", "toolz", "tests"))
    with open(os.path.join(base, "grader", "rubric.txt"), "w") as f:
        f.write("\n".join(rubric_line(t) for t in bug["tests"]) + "\n")
    with open(os.path.join(base, "prompt.txt"), "w") as f:
        f.write(PROMPT_TMPL.format(symptom=bug["prompt"]))
    return bug["id"]


def _run_tests(repo_dir, tests):
    sel = " or ".join(tests)
    r = subprocess.run(
        ["uv", "run", "--no-project", "--with", "pytest", "python", "-m", "pytest",
         f"{TESTS_DIR}/test_itertoolz.py", "-k", sel, "-q"],
        cwd=repo_dir, capture_output=True, text=True,
    )
    return r.returncode, r.stdout + r.stderr


def self_test(repo):
    for bug in BUGS:
        with tempfile.TemporaryDirectory() as d:
            shutil.copytree(os.path.join(repo, "toolz"), os.path.join(d, "toolz"))
            # clean → the bug's tests must PASS
            rc, out = _run_tests(d, bug["tests"])
            assert rc == 0, f"{bug['id']}: clean toolz failed its own tests:\n{out[-800:]}"
            # bugged → they must FAIL (the bug is real + caught)
            _patch_file(os.path.join(d, "toolz", "itertoolz.py"), bug)
            rc, out = _run_tests(d, bug["tests"])
            assert rc != 0, f"{bug['id']}: bugged toolz still PASSED — the bug isn't caught:\n{out[-800:]}"
        print(f"  ok {bug['id']}: clean passes {bug['tests']}, bugged fails them")
    print(f"self-test ok: {len(BUGS)} real-bug tasks validated (no agent)")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("outdir", nargs="?", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "toolz-tasks"))
    ap.add_argument("--repo")
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args()

    repo = ensure_repo(args.repo)
    if args.self_test:
        self_test(repo)
        return
    os.makedirs(args.outdir, exist_ok=True)
    ids = [build_task(repo, args.outdir, b) for b in BUGS]
    print(f"wrote {len(ids)} real-bug tasks to {args.outdir}: {', '.join(ids)}")
    print(f"freeze: scripts/eval/freeze-split.sh {args.outdir} toolz")


if __name__ == "__main__":
    main()
