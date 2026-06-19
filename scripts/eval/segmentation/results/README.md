# σ̂-segmentation results

Durable trial records from the segmentation harness (`../run.sh`). Each file is
JSONL, one record per trial: `{task, cond, trial, score, cost}`. The verdict that
consumes them lives in [`docs/optimization-gate.md`](../../../../docs/optimization-gate.md)
(the σ̂-segmentation keystone section).

| file | run | n/arm | model | headline |
|---|---|---:|---|---|
| `ap_pov-glm51-n10.jsonl` | GHOST-007, 2026-06-14 | 10 | zai-coding-plan/glm-5.1 | σ̂ 0.467 → 0.000 (monolithic → segmented); mean 0.42 → 1.00 |
| `h1-3task-glm51-n10.jsonl` | H1 multi-task, 2026-06-15 | 10 | zai-coding-plan/glm-5.1 | 3 tasks (dot_dsl/grade_school/pov); pooled σ̂ 0.212 → 0.026; paired lift +0.41, CI [0.25, 0.53] excludes zero |
| `h2-segretries0-glm51-n10.jsonl` | H2 retry-isolation, 2026-06-15 | 10 | zai-coding-plan/glm-5.1 | same 3 tasks, `SEG_RETRIES=0`; σ̂ 0.251 → 0.037, lift +0.25 [0.13, 0.33] still excludes zero → retry not the driver |
| `h4-horizon-reset-glm51-n10.jsonl` | H4 reset-vs-scope, 2026-06-15 | 10 | zai-coding-plan/glm-5.1 | 3 arms (monolithic/chained/segmented); σ̂ 0.198/0.055/0.052; scope alone +0.52 [0.23,0.69], horizon-reset-on-top −0.025 [−0.075,0.0] → **scope is the mechanism, session reset adds nothing** |
| `enum-control-3task-glm51-n10.jsonl` | enumerated-monolithic control (`ENUM_MONO=1`), 2026-06-19 | 10 | zai-coding-plan/glm-5.1 | 4 arms; σ̂ mono 0.305 / enum 0.109 / chained 0.045 / seg 0.057; prompt-decomp (enum−mono) +0.17, Δσ̂ −0.196; checkpoint-gating (chained−enum) +0.18, Δσ̂ −0.064, pass 11→19/30 → **gating is a real, separable lever — "just a better prompt" refuted** |

Re-derive the stats from any file:

```sh
python3 ../../paired-stats.py --baseline monolithic --treatment segmented ap_pov-glm51-n10.jsonl
```

Reproduce a run: see the "Reproduce" block in the verdict doc. Run from an **immune
binary copy** — an external `cargo build` can clobber `target/debug` without the
libkrun feature → silent docker fallback → cost-0 / no-workspace zeros. And **reap
VMs between large batches**: a 30-session libkrun run can exhaust fresh-VM boot
capacity until resources are reclaimed (observed in the GHOST-007 run; the completed
batch was unaffected — verified by zero cost-0 records).
