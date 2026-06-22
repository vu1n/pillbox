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
| inject playbook | prepend task-relevant claims to the prompt | **`kypp recall <task>`** (compose) |
| Generator | run the worker, grade it | `pillbox run` + `session score` |
| Reflector | mine the failure trajectory → lessons | **`kypp capture --distill`** |
| Curator | dedup / promote-corroborated / supersede | **`kypp consolidate`** |
| helpful/harmful | attribute the score to the claims a run saw | **`kypp usage`** + score |

ACE bullets become kypp claims, inheriting governance (authority, corroboration,
staleness) that AxACE's flat playbook lacks.

Injection is task-conditioned `recall` by default, **not** `briefing` (dump-all):
dumping the full store pollutes a cheap model (scores below baseline — kypp handoff
`HANDOFF-kypp-kimi.md` §2.1). `--inject {recall,briefing,none}` selects compose /
dump-all / baseline for the ablation; semantic targeting needs `KYPP_EMBED_MODEL`.

**What it measures:** held-out quality as the playbook grows over iterations — the
*accrual* question ("does remembering lessons help?"), kept honest by a fixed held-out
split the loop never reflects on. This is **not** a quality-lift claim against the σ̂
wall (the parked optimization gate); it's the runtime-memory question the memory matrix
(`../eval/memory/`) validated at the single-task level, now in a loop.

**Curator remove-harmful (credit-assignment #2):** `ace.py` computes a per-claim
helpful/harmful signal **ghost-side** from `kypp usage` (which claims a run saw) + the
run score, flags harmful candidates, and `kypp reject <handle>` (landed) does the demote
(status=rejected → dropped from recall/briefing, row preserved). Gated behind
`--prune-harmful` (off by default — the attribution is correlational, so don't auto-prune
unsupervised on one round's evidence).

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
