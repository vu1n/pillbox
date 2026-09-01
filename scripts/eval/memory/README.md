# Memory-validity matrix

Measures whether kypp memory actually changes agent behavior — the experiment the
optimization gate couldn't be. Each task's correct answer is **out-of-band** (it lives
only in memory, never in the workspace), so a memory-OFF run genuinely can't solve it
and any memory-ON success is attributable to recall, not competence. A planted answer
+ a near-binary grader sidesteps the variance wall that killed lift measurement (see
`docs/optimization-eval-family.md`).

One task per kypp validity lever:

| Lever | What it tests | Pass condition |
|---|---|---|
| `recency` | a stale fact, then a correction | brief surfaces the NEW value, not the stale one |
| `authority` | agent guess (candidate) + human fact coexist | recall prefers the human value |
| `corroboration` | two independent sessions agree; one dissents | the corroborated value wins; the dissenter (single-source candidate) stays out of the brief |
| `scope` | a fact in a DIFFERENT project | it does NOT leak into this project's brief (false-application) |
| `pitfall` | a negative lesson ("X fails, use Y") | the agent avoids X |

## Run

```sh
# 1. generate the family
python3 gen-memory-tasks.py

# 2. the gate-before-the-gate — validate WITHOUT an agent: out-of-band integrity +
#    the seed produces the intended brief at the kypp level. If this fails, no agent
#    run is worth doing.
python3 gen-memory-tasks.py --self-test

# 3. plumbing check — seed + brief + composed prompt, still no agent
python3 memory-matrix.py --dry-run

# 4. the measurement (needs a codesigned libkrun binary + opencode authed + runner
#    image, exactly like scripts/smoke/) — three arms per lever:
scripts/lk-build.sh                       # from repo root: build + codesign
PILLBOX=./target/debug/pillbox \
  python3 memory-matrix.py --trials 3 --out mem-run.json
```

## Arms

- `off` — no memory. The floor; `app_rate` here should be ~0 (out-of-band). If it
  isn't, the task leaks — reported loudly, because it invalidates the lever.
- `on` — the lever's memory seeded, brief prepended. `lift = on − off` is the effect.
- `distractor` — `on` plus irrelevant noise claims. `distractor < on` means context
  pollution is crowding out the signal.

## Verdict logic

- lift levers: `MEMORY WORKS` when `(on − off) > 0.5` and `off < 0.25`.
- `scope`: `leak_rate(on) < 0.25` → `SCOPE HOLDS`.

## Why we inject the brief ourselves

The harness runs `kypp briefing` and prepends it to the prompt rather than using
`pillbox run --memory`. Same payload, but it isolates the memory-validity variable
from pillbox's brief plumbing (the single-positional heuristic, project derivation) —
we're testing kypp's levers, not the glue. `_seed_runner.py` is the one place that
touches the `kypp.store` API (the cost-router reuses it for recording outcomes).
