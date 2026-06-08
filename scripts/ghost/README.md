# ghost — the meta-harness

ghost is the orchestrating layer: it decides *how* to run a task and *learns* from
the outcome. It is a **consumer** of two services, not a substrate itself:

- **kypp** (`~/code/kypp`) — the governed memory engine (ghost's `recall`/`brief`).
- **pillbox** — the substrate (sandbox, §0 log, the verifiable `session score`, cost).

> Naming: *ghost = meta-harness*, *kypp = its memory engine*. Distinct components.

Two modes today, both reusing `../eval/gate.py`'s proven run→score→frozen-task substrate:

## `ghost.py` — router (v1)
Per-task model selection. Policies `always:<model>` / `cascade:<m1,m2,…>`; metric =
**cost-adjusted quality**; computes an oracle ceiling (is a learned router even worth
building on this set?). See the file header. The cost-router in `../router/` is the
memory-backed sibling (learns which model clears the bar per task-class).

## `ace.py` — ACE loop over kypp
[ACE](https://arxiv.org/abs/2510.04618) (Agentic Context Engineering): a
Generator→Reflector→Curator loop that **grows a playbook from execution feedback**
rather than rewriting a prompt. Every stage is a kypp verb, and the playbook *is*
kypp's governed memory (no second store):

| ACE stage | ghost does | kypp verb |
|---|---|---|
| inject playbook | prepend the digest to the prompt | **`kypp briefing`** |
| Generator | run the worker, grade it | `pillbox run` + `session score` |
| Reflector | mine the failure trajectory → lessons | **`kypp capture --distill`** |
| Curator | dedup / promote-corroborated / supersede | **`kypp consolidate`** |
| helpful/harmful | attribute the score to the claims a run saw | **`kypp usage`** + score |

ACE bullets become kypp claims, inheriting governance (authority, corroboration,
staleness) that AxACE's flat playbook lacks.

**What it measures:** held-out quality as the playbook grows over iterations — the
*accrual* question ("does remembering lessons help?"), kept honest by a fixed held-out
split the loop never reflects on. This is **not** a quality-lift claim against the σ̂
wall (the parked optimization gate); it's the runtime-memory question the memory matrix
(`../eval/memory/`) validated at the single-task level, now in a loop.

**The named gap:** the Curator's *remove-harmful* needs a per-claim helpful/harmful
signal — credit-assignment #2, never built in kypp. The pieces exist (`kypp usage`
records which claims a run saw; the run has a score), so `ace.py` computes the
attribution **ghost-side** and reports harmful candidates. Acting on them (supersede)
is gated behind `--prune-harmful` (off by default — destructive, correlational, needs
more than one round of evidence).

### Run
```sh
python3 ghost/ace.py --self-test          # attribution math, no agent

scripts/lk-build.sh                        # codesigned libkrun binary (from repo root)
KYPP_DISTILL_MODEL=zai-coding-plan/glm-5.1 \
PILLBOX=./target/debug/pillbox \
  python3 ghost/ace.py --train aider --iters 3 \
    --worker-model zai-coding-plan/glm-4.5-air --project ace-aider
```
Prereqs (same as `gate.py`): frozen `aider/{train,held-out}/*` bookmarks in the `evals`
pillbox (`../eval/freeze-split.sh`), opencode authed, runner image present, `kypp` on
PATH. The Reflector's distiller model is kypp's `KYPP_DISTILL_MODEL` env (a frontier
reflector over a cheap worker = the teacher→student split).
