# Cost router

Routes a coding task to the **cheapest model that clears the bar**, and learns which
model that is by remembering outcomes in kypp.

The measurable half of the routing story. "Which model is *better*" is the same fuzzy
quality delta that drowned the optimization gate in variance — don't learn it. "Did
the cheaper model *clear the bar*" is binary (the verifiable rubric/cmd grade) and
cost is observable (the §0 `usage` events). So the learnable policy is: route to the
cheapest model whose adequacy for this task **class** is corroborated; otherwise
explore the next-cheapest; record every outcome.

**The routing policy IS memory.** Each outcome is a kypp claim on subject
`route/<class>/<model>`. Two independent passes (distinct session source_ids)
consolidate to `accepted` — kypp's corroboration lever — and the router then treats
the model as adequate-for-the-class and stops exploring. The same validity levers the
memory matrix tests govern routing: a model that regresses can be `kypp correct`'d
back to inadequate. pillbox exposes the signals; kypp accrues the policy; this reads
it. No optimizer inside pillbox.

## Run

```sh
# inspect the learned policy (no run)
python3 cost-router.py --class py-bugfix --explain

# route one task: explores cheapest→up, grades against the hidden bar, records outcomes
scripts/lk-build.sh                        # from repo root: codesigned libkrun binary
PILLBOX=./target/debug/pillbox \
  python3 cost-router.py --class py-bugfix \
    --task-dir ../eval/memory/tasks/pitfall \
    --ladder zai-coding-plan/glm-4.5-air,zai-coding-plan/glm-5.1
```

A task dir is the eval-rig format: `prompt.txt`, `workspace/`, `grader/` (with
`rubric.txt` or `grade.sh`). Frozen-bookmark tasks work via `pillbox pull` into a dir
first.

## The demonstration

Run the same class twice while it passes on the cheap model: after the 2nd pass the
cheap model consolidates to `ADEQUATE`, so subsequent routes try it first and stop —
**cost drops without quality dropping** (the grade still gates). Watch the policy form
with `--explain` between runs.

## Pricing

Cost is summed from the §0 usage events via the same `PRICE_*_PER_M` env vars as
`scripts/eval/lib.sh` (`pb_usage`). Set them to the eval models' real rates. The
ladder is ordered cheapest→most-capable by construction (`--ladder` or `ROUTER_LADDER`);
the router prefers the earliest adequate model.

## Status

Recall → corroboration → route-order is verified end-to-end without an agent (one pass
= candidate, two = adequate, a failure orders the model last). The live agent leg needs
a codesigned libkrun box (like `scripts/smoke/`): opencode authed, models reachable,
runner image present.
