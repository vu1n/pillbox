# Prove-the-loop eval rig

The cheap, mostly-external experiment from [swarm-memory.md](../../docs/swarm-memory.md)
§ "prove it before you build it": **does injected memory improve the task pass
rate?** If it doesn't beat baseline, neither the run-time (ACE) nor the
compile-time (GEPA) optimization loop is worth building — so run this *before*
investing in more substrate.

Zero pillbox code: the scripts consume `pillbox run` / `session send` /
`session score` (the verifiable reward channel) externally.

## What it does

For each task under `tasks/`, both conditions are run + graded:

- **baseline** — send the task prompt.
- **memory** — prepend `memory/playbook.md` (hand-written bullets) to the prompt.

The agent (opencode, libkrun) edits a CoW clone of the task workspace; the
task's `grade.sh` is run against the result by `session score` (exit 0 → pass,
recorded as a verifiable `scored` §0 event). `run-ab.sh` tabulates pass rates.

```sh
# build + sign the libkrun binary first (see docs/libkrun-sandbox.md), then
# populate tasks/ from a benchmark and A/B it:
python3 scripts/eval/import-aider-polyglot.py --limit 20   # recommended set
PILLBOX_RUNNER_IMAGE=pillbox-runner:l7 scripts/eval/run-ab.sh 5
```

## A task = a directory under `tasks/`

- `workspace/` — the agent's starting tree (e.g. `solution.py`), copied pristine
  per run. **This is all the agent sees.**
- `grader/` — the verifier, **hidden from the agent** (injected into the edited
  clone only at grade time, so it can't read the test and hardcode). `grade.sh`
  is the entry point: cwd = the edited workspace, exit 0 = pass, stdout is the
  feedback gradient. Use only host-available tools.
- `prompt.txt` — the instruction (no test leaked).

The bundled `add` task is a **plumbing smoke** (trivial → both conditions pass →
no signal).

## Populating tasks from a benchmark

Two importers emit the layout above (one dir per problem, hidden grader).
Generated `tasks/he_*` / `tasks/ap_*` + the `.cache/` clone are gitignored —
regenerate them, don't commit.

- **`import-aider-polyglot.py`** (recommended) — the Aider polyglot **Python**
  track (Exercism problems, `unittest`-graded on the host via `python3 -m
  unittest discover`). Less contaminated + more agentic than HumanEval, and
  non-trivial (`beer_song` baseline fails GLM — room for memory to help). Clones
  Aider-AI/polyglot-benchmark to `.cache/` on first run.
- **`import-humaneval.py`** — HumanEval (function-completion, the problem's own
  `check()` as grader). Easiest to fetch, but ⚠️ **heavily contaminated** + weak
  tests → the baseline is inflated, so memory has little room to move it. Use it
  to prove the harness *runs*, not for a trustworthy signal.

For the rigorous run — **SWE-rebench / SWE-bench-CL** (the continual-learning
bench, the closest analog to the memory loop) — you need a **sandboxed grader**
(those tasks grade inside a per-task env; `session score` runs host-side today)
+ a capable model (GLM-4.5-air floors them). The dir layout is identical; only
the importer + the grader location change.

## Status / findings (2026-06-02)

- ✅ **Full harness verified live on a real benchmark task**: `import-humaneval`
  → opencode completed `has_close_elements` in the VM → the hidden grader
  (injected at grade time) verified it → `session score` → verifiable pass in
  the §0 log (`run-task.sh he_HumanEval_0 baseline` → pass). The agent never saw
  the test.
- ⚠️ **Trace persistence is watcher-dependent** (matters for the *optimization*
  step, not this pass-rate A/B): the libkrun §0 conversation trace is drained
  into `log.jsonl` only while a `watch`/`subscribe` is attached. A batch
  `run→send→score` leaves the conversation in the raw `/event` capture file but
  not the durable log. GEPA/ACE want the trace persisted without a live watcher
  — the "always-on §0 drain" gap. The reward (score) is unaffected (it's
  appended directly).
- The A/B's bottleneck is now **task curation + model budget**, not pillbox
  plumbing.
