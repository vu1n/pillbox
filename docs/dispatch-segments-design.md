# Design: `pillbox dispatch --segments` — promote the proven segmentation lever

**Status: BUILT + LIVE-VERIFIED (2026-06-19).** Implemented in
`src/commands/dispatch.rs` (`--segments SPEC`, the `Grader` seam,
`drive_segments_inner`, TOML `load_segments`, the additive `segments` verdict array)
+ `src/main.rs`; 22 dispatch unit tests pass (5 new). Docs in `docs/dispatch.md`.
**Live e2e: `scripts/smoke/dispatch-segments.sh` PASS** on libkrun (l7, glm-4.5-air,
k=1) — 1 worker drove the 2-segment chain in one session, both gates passed, the
run-level reward selected it, `a.txt`+`b.txt` pulled. GHOST-005 (skill + rubric
library) also shipped. The gate that licensed this build: the enumerated-monolithic
control (`docs/optimization-gate.md` §2026-06-19) — checkpoint-gating is a real,
separable lever (+0.18 mean lift, −0.064 σ̂-cut on top of the decomposed prompt; pass
11→19/30). Single-model caveat (glm-5.1) stands; H5 is the breadth check, not a build
blocker. **Remaining:** measure best-of-k (the fork-`k` diversity axis), then H5.

The design below is **as built** (open decisions resolved: TOML spec, final reward
required, per-segment retry).

## Why

The σ̂ experiments (`docs/optimization-gate.md`, H1–H4) concluded that the
*segmentation* lever — what cuts trial-to-trial variance and lifts the mean — is
**in-session focused-prompt chaining with per-checkpoint verification**, NOT
fork-per-segment / fresh sessions (H4: the `chained` arm captured 98% of the
σ̂-cut and 105% of the lift; horizon-reset on top added nothing).

That lever exists **only** as `run_chained_cell` in the eval harness
(`scripts/eval/segmentation/run.sh`). The shipped `pillbox dispatch` is fork-`k`
only; `pillbox eval` is single-shot. So the workstream's headline result is
**unshipped** — no reusable verb does in-session chaining. This design promotes it.

The two levers are **orthogonal axes** and should compose, not compete:

| axis | mechanism | shipped today | this design |
|---|---|---|---|
| **segmentation** | focused checkpoint-gated sub-prompts, ONE session | no (only in run.sh) | `dispatch --segments` |
| **diversity** (best-of-k) | `k` independent attempts at one horizon → select best | yes (`dispatch -k`) | composes: `--segments -k N` |

## The gate — CLEARED 2026-06-19

The conditions that gated this (don't build ahead of evidence — the exact failure
the 2026-06-19 ultra-review flagged):

1. **The enumerated-monolithic control — RESOLVED, build justified.** The
   `ENUM_MONO=1` arm split `chained − monolithic` into prompt-decomposition
   (`enumerated − monolithic`) and checkpoint-gating (`chained − enumerated`).
   Result (3 tasks, glm-5.1, n=10; `docs/optimization-gate.md` 2026-06-19):
   `chained ≫ enumerated` — gating adds **+0.18 mean lift** and an additional
   **−0.064 σ̂-cut** on top of the decomposed prompt, lifting pass-rate 11→19/30
   (helps on the 2 tasks with headroom; neutral only on the one enumerated already
   saturates). The "just a better prompt" hypothesis is refuted → **build this
   verb.** (Build BOTH halves: the prompt-decomposition half is free — ship it as a
   skill too.)
2. **The σ̂ result is still single-model (glm-5.1).** H5 (cross-model) does not
   block *building* the verb, but does block *trusting magnitudes as load-bearing*.
   Build now; validate breadth after.

## Contract (when the gate clears)

A new mode of `dispatch`, additive to the existing flags and `--json` envelope.

```
pillbox dispatch --from-bookmark NAME --segments SPEC \
  (--rubric FINAL | --cmd "FINAL_VERIFIER") \
  [-k N] [--retries N] [--ttl D] [--agent A] [--model M] [--temperature F] \
  [--memory] [--json] -- "<overall task prompt / context>"
```

- `--segments SPEC` switches dispatch from "one prompt, fork-`k`" to "drive an
  ordered segment chain." Mutually compatible with `-k` (each of the `k` workers
  runs the **full** chain in its own session; select the best by the FINAL grade —
  best-of-k over segmented chains).
- The existing `--rubric`/`--cmd` stays the **final reward** (the authoritative,
  Goodhart-safe grade that selects the winner and is reported as `score`). The
  per-segment gates live *in the spec*.
- `--retries` becomes the **per-segment** gate-retry budget (distilled feedback
  fed back as the next prompt within that segment), matching today's per-worker
  semantics.

### Segment spec format

TOML (consistent with `pillbox eval`'s spec style). Each segment is a focused
sub-prompt + a gate (rubric subset, or a command):

```toml
[[segment]]
name        = "reroot"
prompt_file = "segments/01-reroot/prompt.txt"   # or: prompt = "..."
gate_rubric = "segments/01-reroot/rubric.txt"   # or: gate_cmd = "pytest -k reroot"

[[segment]]
name   = "pathfind"
prompt = "Implement Tree.path_to(from_node, to_node) ..."
gate_cmd = "python3 -m pytest -k path"
```

Gates are **self-contained** — they run against the worker's live workspace as-is
(same as today's `dispatch --rubric`). This is the deliberate boundary from the
eval harness: the eval family injects *hidden* test subsets at grade time (which
is why `run.sh` reuses dispatch's primitives instead of shelling `dispatch`); the
shipped verb is for **real work** whose tests live in the workspace. The harness
stays separate.

### Drive model (one worker = the `chained` arm)

Per worker, in ONE session (no horizon reset — H4 showed reset adds nothing and
costs a hair):

1. Boot one session from `--from-bookmark` (the existing `fork`).
2. For each segment in order:
   a. `send` the focused sub-prompt → `wait_idle`.
   b. Grade against the segment's gate (rubric/cmd).
   c. On fail with budget left: `send` the distilled failure summary
      (`distill_feedback`, reused), re-grade — up to `--retries` times.
   d. Advance (context accumulates; the session is **not** recycled).
3. After the last segment: grade against the FINAL `--rubric`/`--cmd` (the
   reward), then `pull` the winner's workspace.

With `-k N`, run `N` such chains; select by the final grade (existing
`select_winner` + tie-break). k=1 (default) = pure segmentation.

### Verdict JSON (additive)

Each worker gains an optional `segments` array — the per-checkpoint trajectory —
leaving every existing field unchanged:

```jsonc
"workers": [
  { "session": "...", "score": 1.0, "passed": true, "retries_used": 0,
    "status": "scored",
    "segments": [                                  // NEW, only with --segments
      { "name": "reroot",   "passed": true, "score": 1.0, "retries_used": 0 },
      { "name": "pathfind", "passed": true, "score": 1.0, "retries_used": 1 }
    ] }
]
```

## Implementation sketch (footprint)

Reuses the existing scaffolding; the only new logic is the per-segment chain
inside one worker's drive.

- `src/commands/dispatch.rs`:
  - `DispatchOpts`: add `segments: Option<PathBuf>`.
  - Generalize the `WorkerDriver::grade` seam to `grade(id, grader)` so a segment
    grade can target the segment's gate while the final grade targets `--rubric`
    (keeps the policy unit-testable over the mock — add a segmented-chain test
    mirroring `loop_selects_winner_and_pulls_it`).
  - Add `drive_segments_inner` (the `chained` loop) alongside `drive_one_inner`;
    `run_dispatch` calls one or the other based on `opts.segments`.
  - Parse + validate the TOML spec (loud error on a missing prompt/gate).
  - Extend `WorkerSummary` / the §0 evidence with the per-segment outcomes.
- `src/main.rs`: `--segments` arg + destructure.
- `docs/dispatch.md`: the `--segments` flag, spec format, the two-axes note,
  verdict addition.
- `.claude/skills/dispatch/SKILL.md` (GHOST-005, still unbuilt): when to segment
  vs fork vs both.

No change to the exit-code contract, the `--json` envelope shape (additive field),
or the existing fork-`k` path.

## Open decisions (resolve at build time)

1. **Spec format**: TOML (above, recommended — matches `eval`) vs the harness's
   `NN-*/{prompt.txt,rubric.txt}` dir layout. A converter could bridge the
   harness specs in `scripts/eval/segmentation/segments/` for dogfooding.
2. **Final grade required?** If `--segments` is given without `--rubric`/`--cmd`,
   fall back to the last segment's gate as the reward, or require an explicit
   final grader? (Recommend: require it — keep the reward distinct from the gates,
   the Goodhart discipline.)
3. **Per-segment vs whole-chain retry**: this design does per-segment retry
   (matches the harness + H2). A whole-chain retry-from-scratch is a separate,
   more expensive option — out of scope for v1.

## Provenance

- Mechanism + evidence: `docs/optimization-gate.md` §H4.
- Review that flagged the unshipped-lever gap + the confound: the 2026-06-19
  ultra-review (memory `ghost-ultra-review-2026-06`).
- The harness reference implementation: `scripts/eval/segmentation/run.sh`
  (`run_chained_cell` / `gate_segment`).
