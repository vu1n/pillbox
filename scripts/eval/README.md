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
# build + sign the libkrun binary first (see docs/libkrun-sandbox.md), then:
PILLBOX_RUNNER_IMAGE=pillbox-runner:l7 scripts/eval/run-ab.sh 5
```

## A task = a directory under `tasks/`

- `prompt.txt` — the instruction sent to the agent.
- `grade.sh` — the verifier (cwd = the edited workspace; exit 0 = pass; stdout
  is the feedback gradient). Use only host-available tools.
- the starting workspace files (e.g. `solution.py`) — copied pristine per run.

The bundled `add` task is a **plumbing smoke** (trivial → both conditions pass →
no signal). Real signal needs tasks where a playbook bullet addresses a gotcha
the baseline agent actually trips on — that curation (your domain) is the
experiment's real work, not the plumbing.

## Status / findings (2026-06-02)

- ✅ **Atomic unit verified live**: task → opencode edits the workspace in the
  VM → `session score` grades the real edited clone → verifiable pass/fail in
  the §0 log.
- ⚠️ **Trace persistence is watcher-dependent** (matters for the *optimization*
  step, not this pass-rate A/B): the libkrun §0 conversation trace is drained
  into `log.jsonl` only while a `watch`/`subscribe` is attached. A batch
  `run→send→score` leaves the conversation in the raw `/event` capture file but
  not the durable log. GEPA/ACE want the trace persisted without a live watcher
  — the "always-on §0 drain" gap. The reward (score) is unaffected (it's
  appended directly).
- The A/B's bottleneck is now **task curation + model budget**, not pillbox
  plumbing.
