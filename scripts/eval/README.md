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
python3 scripts/eval/import-humaneval.py --limit 20
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

`import-humaneval.py` pulls HumanEval into the layout above (one dir per problem,
hidden grader). Generated `tasks/he_*` / `tasks/ap_*` are gitignored — regenerate
them, don't commit them.

⚠️ **Benchmark choice drives signal.** HumanEval is heavily contaminated (models
memorized it) and its tests are weak — fine for proving the harness *runs* an
A/B, but the baseline is inflated so memory has little room to help. For a
trustworthy signal graduate to a less-contaminated, agentic set: **Aider
polyglot** (Exercism problems, `unittest`-graded, host-runnable — the next
importer) or, with a sandboxed grader + a capable model, **SWE-rebench /
SWE-bench-CL** (the continual-learning bench — the closest analog to the memory
loop). The dir layout is identical; only the importer swaps.

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
