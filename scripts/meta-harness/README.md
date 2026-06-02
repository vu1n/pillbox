# Meta-harness — eval-driven instruction-profile optimizer (GEPA-lite)

The optimization layer (the "meta-harness" in [vnext.md](../../docs/vnext.md)'s
terms — distinct from the multiplayer *gateway*): it wraps the agent and makes
runs *rip* by improving the instruction profile it runs with, scored against the
eval rig + the verifiable reward channel. An external orchestrator — it *consumes*
pillbox (`run` / `session send` / `session score`), it isn't pillbox core.

## The loop (`optimize.sh`)

```
round r:  eval(best_profile) on TRAIN tasks  ── capturing failures (run-task FAILDIR)
          → propose.sh reflects on the failures → a candidate profile
          → eval(candidate) on a DISJOINT HELD-OUT set
          → keep iff held-out score improves
```

The **held-out split is the point**: the profile is distilled from TRAIN
failures but scored on tasks it never saw, so a gain is *generalization*, not
teaching-to-the-test. The profile is injected by prepending it to the task
prompt (the proven mechanism from the eval A/B).

```sh
# build+sign the libkrun binary, import a task pool, then:
python3 ../eval/import-aider-polyglot.py --limit 12
PILLBOX_RUNNER_IMAGE=pillbox-runner:l7 MODEL=zai-coding-plan/glm-5.1 TRIALS=3 ./optimize.sh --rounds 2
```

## `propose.sh` — the self-improvement core (validated)

Reflects on eval failures and writes an improved, *general* profile — itself an
opencode session (reuses pillbox's auth/egress/§0; no new model integration).

**Validated 2026-06-02:** given the real `beer_song` failure (returned verses as
multi-line strings; the test wants a flat list of lines with `''` separators),
`glm-5.1` reflection autonomously produced a profile that **rediscovered the
hand-written lesson** ("return each line as its own element, never join with
`\n`"; "empty-string separators") *and* generalized it (edge-case wording,
pluralization, range/boundary, return the documented type) — without hardcoding
the answer. The "failures → better general instructions" step works.

## Status + the load-bearing caveat

- ✅ All three pieces validated independently: **eval** (the A/B: baseline 3/6,
  memory 4/6 on glm-5.1, the flip = the task the bullet addressed), **propose**
  (above), **select** (a held-out comparison). The full `optimize.sh` loop is
  orchestration over these — runnable, not yet run end-to-end (it's
  budget-heavy: each round ≈ |train| + 1 + |heldout| agent sessions × `TRIALS`).
- ⚠️ **The model is stochastic — use `TRIALS>=3`.** Live, `glm-5.1` flipped
  `beer_song`/`book_store` pass↔fail across runs; single-trial verdicts wobble,
  so `select` would otherwise pick a candidate on noise. A stable held-out
  pass-rate needs several trials per task (multiplies cost accordingly).

## Relationship to swarm memory (ACE)

Same machinery, different "memory": here `propose` distills ONE global profile
(GEPA-style instruction optimization). The ACE/swarm-memory sibling distills
PER-TASK bullets retrieved on demand (next: mem0 + the always-on §0 drain so the
loop runs over real session traces, not hand-fed failure reports). Both inject
the same way and are scored by the same rig.
